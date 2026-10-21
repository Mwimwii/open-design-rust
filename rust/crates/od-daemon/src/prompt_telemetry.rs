//! Prompt-stack telemetry (parity: `apps/daemon/src/prompt-telemetry.ts`).
//!
//! The builder turns a composed prompt plus its named sections into a bounded,
//! redacted observation: local paths and secrets are scrubbed before hashing,
//! content capture is per-kind, each section is capped, and the aggregate
//! redacted body has its own budget. Section allocation follows the TS
//! priority order so the most diagnostically useful sections survive a budget
//! squeeze.
//!
//! Not ported here: `bindOdNextExactSendPromptEvidence` /
//! `assertOdNextExactSendPromptEvidence`. They bind the OD Next strategy task
//! store's persisted `{kind, schema, text, utf8Bytes, sha256}` identity, and
//! that store is not ported yet. The builder they wrap is ported, so the
//! binding can land with the strategy lane without redoing this module.
//!
//! Fingerprints are SHA-256 over the redacted bytes, exactly like TS. Object
//! key order in the metadata summaries follows `serde_json`'s sorted maps
//! rather than TS insertion order, so a fingerprint is stable across Rust
//! builds but is not byte-identical to the TS one; the payload *shape* is.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::redact::redact_secrets;

pub const PROMPT_STACK_REDACTION_VERSION: &str = "prompt-stack-redaction-v1";
pub const PROMPT_STACK_PATH_MARKER: &str = "[REDACTED:path]";

const KIB: usize = 1024;
const DAEMON_SYSTEM_PROMPT_MAX_BYTES: usize = 128 * KIB;
const SECTION_MAX_BYTES: usize = 64 * KIB;
const TOTAL_REDACTED_CONTENT_MAX_BYTES: usize = 512 * KIB;
const CHILD_PROMPT_MESSAGE_MAX_BYTES: usize = 16 * KIB;
const CHILD_PROMPT_TOTAL_MAX_BYTES: usize = 64 * KIB;
const CHILD_PROMPT_MAX_MESSAGES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PromptTelemetrySectionKind {
    FormOverride,
    DaemonSystemPrompt,
    RuntimeToolPrompt,
    ResearchCommandContract,
    RunContextPrompt,
    ClientSystemPrompt,
    EchoGuard,
    UserRequest,
    SkillPrompt,
    DesignSystemPrompt,
    PluginStagePrompt,
    OdNextExactFinalText,
    CwdHint,
    LinkedDirsHint,
    Attachments,
}

impl PromptTelemetrySectionKind {
    fn captures_redacted_content(self) -> bool {
        matches!(
            self,
            Self::FormOverride
                | Self::DaemonSystemPrompt
                | Self::RuntimeToolPrompt
                | Self::ResearchCommandContract
                | Self::RunContextPrompt
                | Self::ClientSystemPrompt
                | Self::EchoGuard
                | Self::UserRequest
                | Self::SkillPrompt
                | Self::DesignSystemPrompt
                | Self::PluginStagePrompt
                | Self::OdNextExactFinalText
        )
    }

    fn priority(self) -> u8 {
        match self {
            Self::FormOverride | Self::OdNextExactFinalText => 1,
            Self::DaemonSystemPrompt => 2,
            Self::RuntimeToolPrompt => 3,
            Self::ClientSystemPrompt => 4,
            Self::SkillPrompt | Self::DesignSystemPrompt | Self::PluginStagePrompt => 5,
            Self::ResearchCommandContract => 6,
            Self::RunContextPrompt => 7,
            Self::EchoGuard => 8,
            Self::UserRequest => 9,
            Self::CwdHint | Self::LinkedDirsHint | Self::Attachments => 99,
        }
    }

    fn per_section_limit(self) -> usize {
        if self == Self::DaemonSystemPrompt {
            DAEMON_SYSTEM_PROMPT_MAX_BYTES
        } else {
            SECTION_MAX_BYTES
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum PromptContentMode {
    #[serde(rename = "redacted-section-content")]
    RedactedSectionContent,
    #[serde(rename = "metadata-only")]
    MetadataOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum TruncationReason {
    #[serde(rename = "section_byte_limit")]
    SectionByteLimit,
    #[serde(rename = "total_budget_exceeded")]
    TotalBudgetExceeded,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptTelemetryInputSection {
    pub kind: PromptTelemetrySectionKind,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub capture_content: Option<bool>,
    #[serde(default)]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptTelemetrySection {
    pub kind: PromptTelemetrySectionKind,
    pub ordinal: usize,
    pub present: bool,
    pub content_mode: PromptContentMode,
    pub raw_bytes: usize,
    pub redacted_bytes: usize,
    pub fingerprint: String,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation_reason: Option<TruncationReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redacted_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptStackTelemetry {
    pub redaction_version: String,
    pub prompt_fingerprint: String,
    pub stack_fingerprint: String,
    pub raw_bytes: usize,
    pub redacted_bytes: usize,
    pub section_count: usize,
    pub redacted_content_bytes: usize,
    pub redacted_content_budget_bytes: usize,
    pub sections: Vec<PromptTelemetrySection>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SafeChildPromptMessage {
    pub ordinal: usize,
    pub raw_bytes: usize,
    pub redacted_bytes: usize,
    pub fingerprint: String,
    pub truncated: bool,
    pub redacted_content: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SafeChildPromptInput {
    #[serde(rename = "type")]
    pub type_name: String,
    pub redaction_version: String,
    pub message_count: usize,
    pub captured_message_count: usize,
    pub raw_bytes: usize,
    pub redacted_content_bytes: usize,
    pub redacted_content_budget_bytes: usize,
    pub truncated: bool,
    pub messages: Vec<SafeChildPromptMessage>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SafeChildPromptTelemetry {
    pub hash: String,
    pub bytes: usize,
    pub safe_payload: SafeChildPromptInput,
    pub truncated: bool,
}

fn sha256_marker(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("sha256:{hex}")
}

fn byte_length(value: &str) -> usize {
    value.len()
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut cut = max_bytes;
    while cut > 0 && (value.as_bytes()[cut] & 0xc0) == 0x80 {
        cut -= 1;
    }
    value[..cut].to_string()
}

fn file_local_path_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?i)(^|[\s(\[{"'`@])file://(?:localhost)?/[^\s)\]}"'`,;<>]+"#)
            .expect("regex")
    })
}

fn posix_local_path_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(^|[\s(\[{"'`@])/(?:Users|home|root|tmp|private/tmp|private/var/folders|private/var/tmp|var/folders|var/tmp|usr/local|opt|Volumes|mnt|media|srv|workspace|workspaces|app)/[^\s)\]}"'`,;<>]+"#,
        )
        .expect("regex")
    })
}

fn windows_local_path_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(^|[\s(\[{"'`@])(?:[A-Za-z]:\\|\\\\)[^\s)\]}"'`,;<>]+"#)
            .expect("regex")
    })
}

pub fn redact_local_paths(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    let marker = PROMPT_STACK_PATH_MARKER;
    let step = |re: &Regex, text: &str| {
        re.replace_all(text, |caps: &regex::Captures<'_>| {
            format!("{}{marker}", &caps[1])
        })
        .to_string()
    };
    let out = step(file_local_path_re(), input);
    let out = step(posix_local_path_re(), &out);
    step(windows_local_path_re(), &out)
}

pub fn redact_prompt_text(input: &str) -> String {
    redact_local_paths(&redact_secrets(input))
}

fn strip_runtime_tool_prompt_tokens(input: &str) -> String {
    input
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .filter(|line| !line.contains("OD_TOOL_TOKEN"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn sanitize_section_content(kind: PromptTelemetrySectionKind, content: &str) -> String {
    let structurally_safe = if kind == PromptTelemetrySectionKind::RuntimeToolPrompt {
        strip_runtime_tool_prompt_tokens(content)
    } else {
        content.to_string()
    };
    redact_prompt_text(&structurally_safe)
}

fn extension_from_path(value: &str) -> Option<String> {
    let base = value.rsplit(['/', '\\']).next().unwrap_or(value);
    let index = base.rfind('.')?;
    if index == 0 {
        return None;
    }
    let ext = &base[index + 1..];
    if ext.is_empty() {
        None
    } else {
        Some(ext.to_lowercase())
    }
}

fn size_bucket(value: f64) -> &'static str {
    if value <= 0.0 {
        "unknown"
    } else if value <= (10 * KIB) as f64 {
        "0-10KiB"
    } else if value <= (100 * KIB) as f64 {
        "10-100KiB"
    } else if value <= (1024 * KIB) as f64 {
        "100KiB-1MiB"
    } else {
        "1MiB+"
    }
}

fn summarize_metadata_value(value: &Value) -> Map<String, Value> {
    let mut summary = Map::new();
    match value {
        Value::Array(items) => {
            let mut extensions = BTreeSet::new();
            let mut size_buckets: BTreeMap<String, usize> = BTreeMap::new();
            let mut selection_kinds: BTreeMap<String, usize> = BTreeMap::new();
            let mut known_size_count = 0usize;
            for item in items {
                match item {
                    Value::String(text) => {
                        if let Some(ext) = extension_from_path(text) {
                            extensions.insert(ext);
                        }
                    }
                    Value::Object(object) => {
                        let file_like = ["filePath", "screenshotPath", "path", "name"]
                            .iter()
                            .find_map(|key| object.get(*key).and_then(Value::as_str))
                            .unwrap_or("");
                        if let Some(ext) = extension_from_path(file_like) {
                            extensions.insert(ext);
                        }
                        if let Some(size) = object.get("size").and_then(Value::as_f64) {
                            if size.is_finite() {
                                known_size_count += 1;
                                *size_buckets
                                    .entry(size_bucket(size).to_string())
                                    .or_insert(0) += 1;
                            }
                        }
                        if let Some(kind) = object.get("selectionKind").and_then(Value::as_str) {
                            *selection_kinds.entry(kind.to_string()).or_insert(0) += 1;
                        }
                    }
                    _ => {}
                }
            }
            summary.insert("count".to_string(), Value::from(items.len()));
            summary.insert(
                "extensions".to_string(),
                Value::Array(extensions.into_iter().map(Value::String).collect()),
            );
            summary.insert(
                "sizeBuckets".to_string(),
                Value::Object(
                    size_buckets
                        .into_iter()
                        .map(|(key, value)| (key, Value::from(value)))
                        .collect(),
                ),
            );
            summary.insert("knownSizeCount".to_string(), Value::from(known_size_count));
            summary.insert(
                "selectionKinds".to_string(),
                Value::Object(
                    selection_kinds
                        .into_iter()
                        .map(|(key, value)| (key, Value::from(value)))
                        .collect(),
                ),
            );
        }
        Value::Object(object) => {
            let keys: Vec<String> = object.keys().cloned().collect();
            summary.insert("keyCount".to_string(), Value::from(keys.len()));
            summary.insert(
                "keys".to_string(),
                Value::Array(keys.into_iter().map(Value::String).collect()),
            );
        }
        Value::String(text) => {
            summary.insert(
                "count".to_string(),
                Value::from(if text.is_empty() { 0 } else { 1 }),
            );
            summary.insert(
                "extensions".to_string(),
                Value::Array(
                    extension_from_path(text)
                        .into_iter()
                        .map(Value::String)
                        .collect(),
                ),
            );
        }
        _ => {}
    }
    summary
}

fn metadata_fingerprint_source(section: &PromptTelemetryInputSection) -> Map<String, Value> {
    match section.metadata.as_ref() {
        Some(value) => summarize_metadata_value(value),
        None => summarize_metadata_value(&Value::String(
            section.content.clone().unwrap_or_default(),
        )),
    }
}

fn section_is_present(section: &PromptTelemetryInputSection) -> bool {
    let raw_content = section.content.as_deref().unwrap_or_default();
    if !raw_content.is_empty() {
        return true;
    }
    match section.metadata.as_ref() {
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Null) | None => false,
        Some(_) => true,
    }
}

/// Build the bounded, redacted observation of one composed prompt.
pub fn build_prompt_stack_telemetry(
    composed_prompt: &str,
    sections: &[PromptTelemetryInputSection],
) -> PromptStackTelemetry {
    let normalized_composed = redact_prompt_text(composed_prompt);
    let raw_bytes = byte_length(composed_prompt);
    let redacted_bytes = byte_length(&normalized_composed);

    struct Built {
        section: PromptTelemetrySection,
        redacted_source: String,
    }

    let mut built: Vec<Built> = sections
        .iter()
        .enumerate()
        .map(|(index, input)| {
            let raw_content = input.content.as_deref().unwrap_or_default();
            let is_content_kind = input.kind.captures_redacted_content();
            let can_capture = is_content_kind && input.capture_content != Some(false);
            let redacted_source = if is_content_kind {
                sanitize_section_content(input.kind, raw_content)
            } else {
                serde_json::to_string(&metadata_fingerprint_source(input))
                    .unwrap_or_else(|_| "{}".to_string())
            };
            let metadata = if is_content_kind {
                input
                    .metadata
                    .as_ref()
                    .filter(|value| value.is_object())
                    .map(summarize_metadata_value)
                    .filter(|summary| !summary.is_empty())
            } else {
                Some(metadata_fingerprint_source(input))
                    .filter(|summary| !summary.is_empty())
            };
            Built {
                section: PromptTelemetrySection {
                    kind: input.kind,
                    ordinal: index,
                    present: section_is_present(input),
                    content_mode: if can_capture {
                        PromptContentMode::RedactedSectionContent
                    } else {
                        PromptContentMode::MetadataOnly
                    },
                    raw_bytes: byte_length(raw_content),
                    redacted_bytes: byte_length(&redacted_source),
                    fingerprint: sha256_marker(&redacted_source),
                    truncated: false,
                    truncation_reason: None,
                    redacted_content: None,
                    metadata,
                },
                redacted_source,
            }
        })
        .collect();

    let mut allocation: Vec<usize> = built
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            entry.section.present
                && entry.section.content_mode == PromptContentMode::RedactedSectionContent
        })
        .map(|(index, _)| index)
        .collect();
    allocation.sort_by_key(|index| {
        let section = &built[*index].section;
        (section.kind.priority(), section.ordinal)
    });

    let mut remaining = TOTAL_REDACTED_CONTENT_MAX_BYTES;
    for index in allocation {
        let entry = &mut built[index];
        if remaining == 0 {
            entry.section.truncated = true;
            entry.section.truncation_reason = Some(TruncationReason::TotalBudgetExceeded);
            continue;
        }
        let per_section = entry.section.kind.per_section_limit();
        let limit = per_section.min(remaining);
        let redacted_content = truncate_utf8(&entry.redacted_source, limit);
        let content_bytes = byte_length(&redacted_content);
        if content_bytes > 0 {
            entry.section.redacted_content = Some(redacted_content);
        }
        remaining -= content_bytes;
        let source_bytes = byte_length(&entry.redacted_source);
        if content_bytes < source_bytes {
            entry.section.truncated = true;
            entry.section.truncation_reason = Some(if limit < per_section {
                TruncationReason::TotalBudgetExceeded
            } else {
                TruncationReason::SectionByteLimit
            });
        }
    }

    let output_sections: Vec<PromptTelemetrySection> = built
        .into_iter()
        .filter(|entry| entry.section.present)
        .map(|entry| entry.section)
        .collect();

    let stack_source: Vec<Value> = output_sections
        .iter()
        .map(|section| {
            serde_json::json!({
                "kind": section.kind,
                "ordinal": section.ordinal,
                "fingerprint": section.fingerprint,
            })
        })
        .collect();
    let stack_fingerprint =
        sha256_marker(&serde_json::to_string(&stack_source).unwrap_or_default());
    let redacted_content_bytes = output_sections
        .iter()
        .map(|section| {
            section
                .redacted_content
                .as_deref()
                .map(byte_length)
                .unwrap_or(0)
        })
        .sum();

    PromptStackTelemetry {
        redaction_version: PROMPT_STACK_REDACTION_VERSION.to_string(),
        prompt_fingerprint: sha256_marker(&normalized_composed),
        stack_fingerprint,
        raw_bytes,
        redacted_bytes,
        section_count: output_sections.len(),
        redacted_content_bytes,
        redacted_content_budget_bytes: TOTAL_REDACTED_CONTENT_MAX_BYTES,
        sections: output_sections,
    }
}

/// Capture the runtime-visible Child task text without retaining its raw body.
pub fn build_safe_child_prompt_telemetry(messages: &[String]) -> SafeChildPromptTelemetry {
    let raw_bytes: usize = messages.iter().map(|message| byte_length(message)).sum();
    let mut remaining = CHILD_PROMPT_TOTAL_MAX_BYTES;
    let mut truncated = messages.len() > CHILD_PROMPT_MAX_MESSAGES;
    let safe_messages: Vec<SafeChildPromptMessage> = messages
        .iter()
        .take(CHILD_PROMPT_MAX_MESSAGES)
        .enumerate()
        .map(|(ordinal, message)| {
            let redacted = redact_prompt_text(message);
            let message_raw_bytes = byte_length(message);
            let redacted_bytes = byte_length(&redacted);
            let limit = CHILD_PROMPT_MESSAGE_MAX_BYTES.min(remaining);
            let redacted_content = truncate_utf8(&redacted, limit);
            let content_bytes = byte_length(&redacted_content);
            remaining = remaining.saturating_sub(content_bytes);
            let message_truncated = content_bytes < redacted_bytes;
            truncated |= message_truncated;
            SafeChildPromptMessage {
                ordinal,
                raw_bytes: message_raw_bytes,
                redacted_bytes,
                fingerprint: sha256_marker(&redacted),
                truncated: message_truncated,
                redacted_content,
            }
        })
        .collect();
    let redacted_content_bytes: usize = safe_messages
        .iter()
        .map(|message| byte_length(&message.redacted_content))
        .sum();
    let hash = sha256_marker(&serde_json::to_string(messages).unwrap_or_default());
    SafeChildPromptTelemetry {
        hash,
        bytes: raw_bytes,
        safe_payload: SafeChildPromptInput {
            type_name: "open-design.child-injected-prompt".to_string(),
            redaction_version: PROMPT_STACK_REDACTION_VERSION.to_string(),
            message_count: messages.len(),
            captured_message_count: safe_messages.len(),
            raw_bytes,
            redacted_content_bytes,
            redacted_content_budget_bytes: CHILD_PROMPT_TOTAL_MAX_BYTES,
            truncated,
            messages: safe_messages,
        },
        truncated,
    }
}

pub fn prompt_stack_without_content(telemetry: &PromptStackTelemetry) -> PromptStackTelemetry {
    let mut without = telemetry.clone();
    without.redacted_content_bytes = 0;
    for section in &mut without.sections {
        section.redacted_content = None;
    }
    without
}

pub fn structured_prompt_stack_input(telemetry: &PromptStackTelemetry) -> Value {
    let sections: Vec<Value> = telemetry
        .sections
        .iter()
        .map(|section| {
            let mut object = Map::new();
            object.insert("kind".to_string(), serde_json::json!(section.kind));
            object.insert("ordinal".to_string(), Value::from(section.ordinal));
            object.insert(
                "contentMode".to_string(),
                serde_json::json!(section.content_mode),
            );
            object.insert("rawBytes".to_string(), Value::from(section.raw_bytes));
            object.insert(
                "redactedBytes".to_string(),
                Value::from(section.redacted_bytes),
            );
            object.insert(
                "fingerprint".to_string(),
                Value::String(section.fingerprint.clone()),
            );
            object.insert("truncated".to_string(), Value::Bool(section.truncated));
            if let Some(reason) = section.truncation_reason {
                object.insert("truncationReason".to_string(), serde_json::json!(reason));
            }
            if let Some(content) = &section.redacted_content {
                object.insert("redactedContent".to_string(), Value::String(content.clone()));
            }
            if let Some(metadata) = &section.metadata {
                object.insert(
                    "metadata".to_string(),
                    Value::Object(metadata.clone()),
                );
            }
            Value::Object(object)
        })
        .collect();
    serde_json::json!({
        "type": "open-design.prompt-stack",
        "redactionVersion": telemetry.redaction_version,
        "promptFingerprint": telemetry.prompt_fingerprint,
        "stackFingerprint": telemetry.stack_fingerprint,
        "sectionCount": telemetry.section_count,
        "redactedContentBytes": telemetry.redacted_content_bytes,
        "redactedContentBudgetBytes": telemetry.redacted_content_budget_bytes,
        "sections": sections,
    })
}

pub fn build_prompt_stack_flat_metadata(telemetry: &PromptStackTelemetry) -> Map<String, Value> {
    let mut metadata = Map::new();
    metadata.insert(
        "promptStack_redactionVersion".to_string(),
        Value::String(telemetry.redaction_version.clone()),
    );
    metadata.insert(
        "promptStack_promptFingerprint".to_string(),
        Value::String(telemetry.prompt_fingerprint.clone()),
    );
    metadata.insert(
        "promptStack_stackFingerprint".to_string(),
        Value::String(telemetry.stack_fingerprint.clone()),
    );
    metadata.insert(
        "promptStack_sectionCount".to_string(),
        Value::from(telemetry.section_count),
    );
    metadata.insert(
        "promptStack_redactedContentBytes".to_string(),
        Value::from(telemetry.redacted_content_bytes),
    );
    metadata.insert(
        "promptStack_redactedContentBudgetBytes".to_string(),
        Value::from(telemetry.redacted_content_budget_bytes),
    );
    metadata
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(kind: PromptTelemetrySectionKind, content: &str) -> PromptTelemetryInputSection {
        PromptTelemetryInputSection {
            kind,
            content: Some(content.to_string()),
            capture_content: None,
            metadata: None,
        }
    }

    #[test]
    fn redacts_paths_and_secrets_before_hashing_or_capture() {
        let telemetry = build_prompt_stack_telemetry(
            "Use /Users/alice/work/repo/index.html with sk-test-1234567890123456789012.",
            &[section(
                PromptTelemetrySectionKind::DaemonSystemPrompt,
                "Read /Users/alice/work/repo/index.html and token sk-test-1234567890123456789012.",
            )],
        );
        let section = &telemetry.sections[0];
        let content = section.redacted_content.as_deref().expect("content");
        assert!(content.contains(PROMPT_STACK_PATH_MARKER));
        assert!(content.contains("[REDACTED:sk_key]"));
        assert!(!content.contains("/Users/alice"));
        assert!(!content.contains("sk-test-"));
        assert!(section.fingerprint.starts_with("sha256:"));
        assert!(telemetry.prompt_fingerprint.starts_with("sha256:"));
    }

    #[test]
    fn normalizes_different_local_paths_to_the_same_fingerprint() {
        let first = build_prompt_stack_telemetry(
            "Open /Users/alice/project/src/App.tsx",
            &[section(
                PromptTelemetrySectionKind::UserRequest,
                "Open /Users/alice/project/src/App.tsx",
            )],
        );
        let second = build_prompt_stack_telemetry(
            "Open /Users/bob/other/src/App.tsx",
            &[section(
                PromptTelemetrySectionKind::UserRequest,
                "Open /Users/bob/other/src/App.tsx",
            )],
        );
        assert_eq!(first.prompt_fingerprint, second.prompt_fingerprint);
        assert_eq!(first.sections[0].fingerprint, second.sections[0].fingerprint);
    }

    #[test]
    fn redacts_private_var_folders_and_root_paths() {
        let telemetry = build_prompt_stack_telemetry(
            "open /private/var/folders/aa/token/T/render.png and /root/project/file.ts",
            &[section(
                PromptTelemetrySectionKind::UserRequest,
                "open /private/var/folders/aa/token/T/render.png and /root/project/file.ts",
            )],
        );
        assert_eq!(
            telemetry.sections[0].redacted_content.as_deref(),
            Some(format!(
                "open {PROMPT_STACK_PATH_MARKER} and {PROMPT_STACK_PATH_MARKER}"
            ).as_str())
        );
    }

    #[test]
    fn normalizes_dev_container_workspace_roots() {
        let first = build_prompt_stack_telemetry(
            "open /workspace/open-design/src/App.tsx",
            &[section(
                PromptTelemetrySectionKind::UserRequest,
                "open /workspace/open-design/src/App.tsx",
            )],
        );
        let second = build_prompt_stack_telemetry(
            "open /workspaces/open-design/src/App.tsx",
            &[section(
                PromptTelemetrySectionKind::UserRequest,
                "open /workspaces/open-design/src/App.tsx",
            )],
        );
        assert_eq!(
            first.sections[0].redacted_content.as_deref(),
            Some(format!("open {PROMPT_STACK_PATH_MARKER}").as_str())
        );
        assert_eq!(first.prompt_fingerprint, second.prompt_fingerprint);
    }

    #[test]
    fn caps_a_section_at_its_byte_limit() {
        let big = "x".repeat(80 * KIB);
        let telemetry = build_prompt_stack_telemetry(
            &big,
            &[section(PromptTelemetrySectionKind::OdNextExactFinalText, &big)],
        );
        let section = &telemetry.sections[0];
        assert_eq!(
            section.redacted_content.as_deref().map(byte_length),
            Some(SECTION_MAX_BYTES)
        );
        assert!(section.truncated);
        assert_eq!(
            section.truncation_reason,
            Some(TruncationReason::SectionByteLimit)
        );
    }

    #[test]
    fn allocates_the_total_budget_by_ts_priority() {
        let big = "y".repeat(120 * KIB);
        let telemetry = build_prompt_stack_telemetry(
            &big,
            &[
                section(PromptTelemetrySectionKind::UserRequest, &big),
                section(PromptTelemetrySectionKind::DaemonSystemPrompt, &big),
                section(PromptTelemetrySectionKind::FormOverride, &big),
            ],
        );
        // formOverride (priority 1) and daemonSystemPrompt (priority 2) each
        // take their 64 KiB / 128 KiB caps, exhausting the 512 KiB budget only
        // after userRequest has had its share.
        assert!(telemetry.redacted_content_bytes <= TOTAL_REDACTED_CONTENT_MAX_BYTES);
        let user_request = telemetry
            .sections
            .iter()
            .find(|section| section.kind == PromptTelemetrySectionKind::UserRequest)
            .expect("user request");
        assert!(user_request.truncated);
    }

    #[test]
    fn metadata_only_kinds_fingerprint_their_summary_and_omit_content() {
        let mut input = section(PromptTelemetrySectionKind::Attachments, "");
        input.metadata = Some(serde_json::json!([
            { "path": "/a/one.png", "size": 2048 },
            { "path": "/a/two.txt", "size": 4096, "selectionKind": "file" }
        ]));
        let telemetry = build_prompt_stack_telemetry("prompt", &[input]);
        let built = &telemetry.sections[0];
        assert!(built.redacted_content.is_none());
        assert_eq!(built.content_mode, PromptContentMode::MetadataOnly);
        let metadata = built.metadata.as_ref().expect("metadata");
        assert_eq!(metadata.get("count"), Some(&serde_json::json!(2)));
        assert_eq!(
            metadata.get("extensions"),
            Some(&serde_json::json!(["png", "txt"]))
        );
        assert_eq!(
            metadata.get("knownSizeCount"),
            Some(&serde_json::json!(2))
        );
    }

    #[test]
    fn capture_content_false_keeps_the_section_but_drops_its_body() {
        let mut input = section(PromptTelemetrySectionKind::SkillPrompt, "secret skill body");
        input.capture_content = Some(false);
        let telemetry = build_prompt_stack_telemetry("prompt", &[input]);
        let built = &telemetry.sections[0];
        assert_eq!(built.content_mode, PromptContentMode::MetadataOnly);
        assert!(built.redacted_content.is_none());
        assert!(built.fingerprint.starts_with("sha256:"));
    }

    #[test]
    fn runtime_tool_prompts_drop_token_lines_before_redaction() {
        let telemetry = build_prompt_stack_telemetry(
            "prompt",
            &[section(
                PromptTelemetrySectionKind::RuntimeToolPrompt,
                "keep me\nOD_TOOL_TOKEN=abc\nalso keep",
            )],
        );
        assert_eq!(
            telemetry.sections[0].redacted_content.as_deref(),
            Some("keep me\nalso keep")
        );
    }

    #[test]
    fn child_prompt_capture_caps_segments_and_total_body() {
        let messages: Vec<String> = (0..20)
            .map(|index| format!("{index}:{}", "x".repeat(24 * KIB)))
            .collect();
        let telemetry = build_safe_child_prompt_telemetry(&messages);
        assert!(telemetry.truncated);
        assert!(telemetry.safe_payload.truncated);
        assert!(telemetry.safe_payload.redacted_content_bytes <= CHILD_PROMPT_TOTAL_MAX_BYTES);
        assert_eq!(
            telemetry.safe_payload.captured_message_count,
            CHILD_PROMPT_MAX_MESSAGES
        );
        assert!(telemetry
            .safe_payload
            .messages
            .iter()
            .all(|message| message.redacted_content.len() <= CHILD_PROMPT_MESSAGE_MAX_BYTES));
    }

    #[test]
    fn child_prompt_capture_redacts_before_hashing() {
        let telemetry = build_safe_child_prompt_telemetry(&[
            "Inspect /Users/alice/private/design.ts with sk-test-1234567890123456789012.".to_string(),
            "Return only the structural result.".to_string(),
        ]);
        let serialized = serde_json::to_string(&telemetry.safe_payload).expect("json");
        assert!(serialized.contains("Inspect"));
        assert!(serialized.contains(PROMPT_STACK_PATH_MARKER));
        assert!(serialized.contains("[REDACTED:sk_key]"));
        assert!(!serialized.contains("/Users/alice"));
        assert!(!serialized.contains("sk-test-"));
    }

    #[test]
    fn without_content_strips_bodies_but_keeps_fingerprints() {
        let telemetry = build_prompt_stack_telemetry(
            "prompt",
            &[section(PromptTelemetrySectionKind::UserRequest, "hello")],
        );
        let without = prompt_stack_without_content(&telemetry);
        assert_eq!(without.redacted_content_bytes, 0);
        assert!(without.sections[0].redacted_content.is_none());
        assert_eq!(
            without.sections[0].fingerprint,
            telemetry.sections[0].fingerprint
        );
    }

    #[test]
    fn structured_input_and_flat_metadata_use_the_contract_keys() {
        let telemetry = build_prompt_stack_telemetry(
            "prompt",
            &[section(PromptTelemetrySectionKind::UserRequest, "hello")],
        );
        let structured = structured_prompt_stack_input(&telemetry);
        assert_eq!(structured["type"], serde_json::json!("open-design.prompt-stack"));
        assert_eq!(structured["sections"][0]["kind"], serde_json::json!("userRequest"));
        assert_eq!(
            structured["sections"][0]["contentMode"],
            serde_json::json!("redacted-section-content")
        );
        let flat = build_prompt_stack_flat_metadata(&telemetry);
        assert!(flat.contains_key("promptStack_promptFingerprint"));
        assert_eq!(
            flat["promptStack_redactionVersion"],
            serde_json::json!(PROMPT_STACK_REDACTION_VERSION)
        );
    }
}
