//! Skill / design-template catalog — parity port of `listSkills`
//! (`apps/daemon/src/skills.ts:232`) and the normalizers it applies to
//! SKILL.md front matter.
//!
//! The Rust daemon serves the headerless lane, so this is
//! `listAllSkills({ workspaceId: null })`: user roots first, then the
//! built-in roots, first id wins, no `workspace_resources` filtering (this
//! daemon has no workspace tables).
//!
//! Two entry shapes come out of here:
//!
//! * [`SkillInfo`] — the full record including the composed `body` and the
//!   on-disk `dir`. `GET /api/skills/:id` serializes it minus `dir`.
//! * [`SkillInfo::summary`] — the listing shape `GET /api/skills` sends:
//!   body and dir stripped, `hasBody` added.
//!
//! Derived example cards (`<parent>:<child>`) are surfaced exactly like the
//! TypeScript listing: one card per `examples/<key>.html` file, inheriting the
//! parent's mode/platform/surface/scenario and full body so "Use this prompt"
//! still composes a real system prompt.
//!
//! DOCUMENTED DEVIATIONS
//!
//! * `String.prototype.slice(0, 320)` counts UTF-16 code units; the prompt
//!   truncation here counts `char`s. The two differ only for astral-plane
//!   characters in the first sentence of a description.
//! * `localeCompare` for example ordering is a plain byte-wise sort, which is
//!   the same order for the ASCII keys `isSafeExampleKey` admits.
//! * `dirHasAttachments` treats a symlinked side file as an attachment by
//!   name (the TypeScript `Dirent.isDirectory()` is false for symlinks too,
//!   so the name test is what decides in both).
//! * Workspace-scoped catalogs (team shares, visibility envelopes) are absent
//!   because there is no workspace identity in this daemon.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::frontmatter::parse_frontmatter;

/// The staged-skill alias directory (`SKILLS_CWD_ALIAS` in cwd-aliases.ts:37).
const SKILLS_CWD_ALIAS: &str = ".od-skills";

/// One catalog entry, matching the TypeScript `SkillInfo` shape.
pub struct SkillInfo {
    pub id: String,
    pub name: String,
    pub display_name: Option<Value>,
    pub description: String,
    pub description_i18n: Option<Value>,
    pub triggers: Vec<Value>,
    pub mode: String,
    pub surface: String,
    pub source: String,
    pub craft_requires: Vec<String>,
    pub platform: Option<String>,
    pub scenario: String,
    pub category: Option<String>,
    pub preview_type: String,
    pub design_system_required: bool,
    pub default_for: Vec<String>,
    pub upstream: Option<String>,
    pub featured: Option<f64>,
    pub fidelity: Option<String>,
    pub speaker_notes: Option<bool>,
    pub animations: Option<bool>,
    pub example_prompt: String,
    pub example_prompt_i18n: Option<Value>,
    pub aggregates_examples: bool,
    pub critique_policy: Option<String>,
    pub body: String,
    pub dir: PathBuf,
}

impl SkillInfo {
    /// The full record minus `dir` (parity: `GET /api/skills/:id`).
    pub fn detail_json(&self) -> Value {
        let mut object = self.summary_object();
        object.insert("body".to_string(), Value::String(self.body.clone()));
        Value::Object(object)
    }

    /// The listing record (parity: `GET /api/skills`, which strips `body` and
    /// `dir` and adds `hasBody`).
    pub fn summary_json(&self) -> Value {
        let mut object = self.summary_object();
        object.insert("hasBody".to_string(), Value::Bool(!self.body.is_empty()));
        Value::Object(object)
    }

    fn summary_object(&self) -> Map<String, Value> {
        let mut object = Map::new();
        object.insert("id".to_string(), Value::String(self.id.clone()));
        object.insert("name".to_string(), Value::String(self.name.clone()));
        if let Some(display_name) = &self.display_name {
            object.insert("displayName".to_string(), display_name.clone());
        }
        object.insert(
            "description".to_string(),
            Value::String(self.description.clone()),
        );
        if let Some(description_i18n) = &self.description_i18n {
            object.insert("descriptionI18n".to_string(), description_i18n.clone());
        }
        object.insert("triggers".to_string(), Value::Array(self.triggers.clone()));
        object.insert("mode".to_string(), Value::String(self.mode.clone()));
        object.insert("surface".to_string(), Value::String(self.surface.clone()));
        object.insert("source".to_string(), Value::String(self.source.clone()));
        object.insert(
            "craftRequires".to_string(),
            Value::Array(
                self.craft_requires
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            ),
        );
        object.insert("platform".to_string(), option_string(&self.platform));
        object.insert("scenario".to_string(), Value::String(self.scenario.clone()));
        object.insert("category".to_string(), option_string(&self.category));
        object.insert(
            "previewType".to_string(),
            Value::String(self.preview_type.clone()),
        );
        object.insert(
            "designSystemRequired".to_string(),
            Value::Bool(self.design_system_required),
        );
        object.insert(
            "defaultFor".to_string(),
            Value::Array(self.default_for.iter().cloned().map(Value::String).collect()),
        );
        object.insert("upstream".to_string(), option_string(&self.upstream));
        object.insert(
            "featured".to_string(),
            match self.featured {
                Some(value) => serde_json::Number::from_f64(value)
                    .map(Value::Number)
                    .unwrap_or(Value::Null),
                None => Value::Null,
            },
        );
        object.insert("fidelity".to_string(), option_string(&self.fidelity));
        object.insert(
            "speakerNotes".to_string(),
            self.speaker_notes.map(Value::Bool).unwrap_or(Value::Null),
        );
        object.insert(
            "animations".to_string(),
            self.animations.map(Value::Bool).unwrap_or(Value::Null),
        );
        object.insert(
            "examplePrompt".to_string(),
            Value::String(self.example_prompt.clone()),
        );
        if let Some(example_prompt_i18n) = &self.example_prompt_i18n {
            object.insert(
                "examplePromptI18n".to_string(),
                example_prompt_i18n.clone(),
            );
        }
        object.insert(
            "aggregatesExamples".to_string(),
            Value::Bool(self.aggregates_examples),
        );
        object.insert(
            "critiquePolicy".to_string(),
            option_string(&self.critique_policy),
        );
        object
    }
}

fn option_string(value: &Option<String>) -> Value {
    value
        .as_ref()
        .map(|text| Value::String(text.clone()))
        .unwrap_or(Value::Null)
}

/// Discover every skill under `roots` (user roots first). Parity:
/// `listSkills` (`skills.ts:232`).
pub fn list_skills(roots: &[PathBuf]) -> Vec<SkillInfo> {
    let mut out: Vec<SkillInfo> = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();

    for (root_index, root) in roots.iter().enumerate() {
        let source = if root_index == 0 { "user" } else { "built-in" };
        let Ok(read_dir) = std::fs::read_dir(root) else {
            continue;
        };
        let mut entries: Vec<_> = read_dir.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() && !file_type.is_symlink() {
                continue;
            }
            let dir = entry.path();
            let skill_path = dir.join("SKILL.md");
            if !skill_path.is_file() {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&skill_path) else {
                continue;
            };
            let parsed = parse_frontmatter(&raw);
            let data = parsed.data;
            let parent_id = data
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| entry.file_name().to_string_lossy().into_owned());
            if seen_ids.contains(&parent_id) {
                continue;
            }
            seen_ids.insert(parent_id.clone());

            let od = data.get("od").and_then(Value::as_object);
            let description = data
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let body = parsed.body;
            let mode = normalize_mode(
                od.and_then(|od| od.get("mode")),
                &body,
                &description,
            );
            let surface = normalize_surface(od.and_then(|od| od.get("surface")), &mode);
            let platform = normalize_platform(
                od.and_then(|od| od.get("platform")),
                &mode,
                &body,
                &description,
            );
            let scenario = normalize_scenario(
                od.and_then(|od| od.get("scenario")),
                &body,
                &description,
            );
            let category = normalize_category(od.and_then(|od| od.get("category")));
            let design_system_required = od
                .and_then(|od| od.get("design_system"))
                .and_then(Value::as_object)
                .and_then(|design_system| design_system.get("requires"))
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let upstream = od
                .and_then(|od| od.get("upstream"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let preview_type = od
                .and_then(|od| od.get("preview"))
                .and_then(Value::as_object)
                .and_then(|preview| preview.get("type"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| "html".to_string());
            let display_name = localized_map_from_fields(
                data.get("en_name"),
                data.get("zh_name"),
            );
            let description_i18n = localized_map_from_fields(
                data.get("en_description"),
                data.get("zh_description"),
            );
            let example_prompt_i18n =
                localized_map_from_record(od.and_then(|od| od.get("example_prompt_i18n")));
            // Parity: the TypeScript listing passes `data.triggers` through
            // unchanged, so a quoted item containing a colon keeps the
            // object shape its YAML subset parser produced for it.
            let triggers: Vec<Value> = data
                .get("triggers")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let parent_body = skill_body_with_root_preamble(&body, &dir);
            let derived_examples = collect_derived_examples(&dir);
            let aggregates_examples = !derived_examples.is_empty();
            let example_prompt = derive_prompt(&data, od);
            let craft_requires = normalize_craft_requires(od.and_then(|od| od.get("craft")).and_then(|craft| craft.get("requires")));
            let default_for = normalize_default_for(od.and_then(|od| od.get("default_for")));
            let featured = normalize_featured(od.and_then(|od| od.get("featured")));
            let fidelity = normalize_fidelity(od.and_then(|od| od.get("fidelity")));
            let speaker_notes = normalize_bool_hint(od.and_then(|od| od.get("speaker_notes")));
            let animations = normalize_bool_hint(od.and_then(|od| od.get("animations")));
            let critique_policy = normalize_critique_policy(
                od.and_then(|od| od.get("critique"))
                    .and_then(|critique| critique.get("policy")),
            );

            out.push(SkillInfo {
                id: parent_id.clone(),
                name: parent_id.clone(),
                display_name,
                description: description.clone(),
                description_i18n: description_i18n.clone(),
                triggers: triggers.clone(),
                mode: mode.clone(),
                surface: surface.clone(),
                source: source.to_string(),
                craft_requires,
                platform: platform.clone(),
                scenario: scenario.clone(),
                category: category.clone(),
                preview_type: preview_type.clone(),
                design_system_required,
                default_for,
                upstream: upstream.clone(),
                featured,
                fidelity: fidelity.clone(),
                speaker_notes,
                animations,
                example_prompt: example_prompt.clone(),
                example_prompt_i18n: example_prompt_i18n.clone(),
                aggregates_examples,
                critique_policy: critique_policy.clone(),
                body: parent_body.clone(),
                dir: dir.clone(),
            });

            for example in derived_examples {
                let derived_id = format!("{parent_id}:{}", example);
                if seen_ids.contains(&derived_id) {
                    continue;
                }
                seen_ids.insert(derived_id.clone());
                out.push(SkillInfo {
                    id: derived_id,
                    name: humanize_example_name(&example),
                    display_name: None,
                    description: description.clone(),
                    description_i18n: description_i18n.clone(),
                    triggers: triggers.clone(),
                    mode: mode.clone(),
                    surface: surface.clone(),
                    source: source.to_string(),
                    craft_requires: Vec::new(),
                    platform: platform.clone(),
                    scenario: scenario.clone(),
                    category: category.clone(),
                    preview_type: preview_type.clone(),
                    design_system_required,
                    default_for: Vec::new(),
                    upstream: upstream.clone(),
                    featured: None,
                    fidelity: fidelity.clone(),
                    speaker_notes,
                    animations,
                    example_prompt: example_prompt.clone(),
                    example_prompt_i18n: example_prompt_i18n.clone(),
                    aggregates_examples: false,
                    critique_policy: critique_policy.clone(),
                    body: parent_body.clone(),
                    dir: dir.clone(),
                });
            }
        }
    }
    out
}

/// Parity: `findSkillById` (`skills.ts:148`) over an already-listed catalog.
pub fn find_skill<'a>(skills: &'a [SkillInfo], id: &str) -> Option<&'a SkillInfo> {
    if id.is_empty() {
        return None;
    }
    let canonical = crate::static_resources::resolve_skill_id(id);
    skills.iter().find(|skill| skill.id == canonical)
}

/// Parity: `collectDerivedExamples` (`skills.ts:464`) — single-file
/// `examples/<key>.html` samples only, sorted for a stable gallery order.
fn collect_derived_examples(dir: &Path) -> Vec<String> {
    let Ok(read_dir) = std::fs::read_dir(dir.join("examples")) else {
        return Vec::new();
    };
    let mut keys: Vec<String> = read_dir
        .flatten()
        .filter(|entry| entry.file_type().map(|kind| kind.is_file()).unwrap_or(false))
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let lower = name.to_lowercase();
            let key = lower.strip_suffix(".html")?;
            if !is_safe_example_key(key) {
                return None;
            }
            Some(key.to_string())
        })
        .collect();
    keys.sort();
    keys
}

/// Parity: `isSafeExampleKey` (`skills.ts:488`).
pub fn is_safe_example_key(key: &str) -> bool {
    if key.is_empty() || key.starts_with('.') || key.contains(':') {
        return false;
    }
    key.chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

/// Parity: `humanizeExampleName` (`skills.ts:497`).
fn humanize_example_name(key: &str) -> String {
    let spaced = key.replace(['-', '_'], " ");
    spaced
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => {
                    let mut out = first.to_uppercase().to_string();
                    out.push_str(&chars.as_str().to_lowercase());
                    out
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parity: `skillBodyWithRootPreamble` (`skills.ts:599`).
fn skill_body_with_root_preamble(body: &str, dir: &Path) -> String {
    if !dir_has_attachments(dir) {
        return body.to_string();
    }
    with_skill_root_preamble(body, dir)
}

/// Parity: `dirHasAttachments` (`skills.ts:611`).
fn dir_has_attachments(dir: &Path) -> bool {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return false;
    };
    read_dir.flatten().any(|entry| {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "SKILL.md" {
            return false;
        }
        entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false)
            || has_side_file_extension(&name)
    })
}

fn has_side_file_extension(name: &str) -> bool {
    let lower = name.to_lowercase();
    [".md", ".html", ".css", ".js", ".json", ".txt"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

/// Parity: `withSkillRootPreamble` (`skills.ts:553`).
fn with_skill_root_preamble(body: &str, dir: &Path) -> String {
    let referenced_files = collect_referenced_side_files(body);
    let folder = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "skill".to_string());
    let normalized_dir = dir.to_string_lossy().replace('\\', "/");
    let digest = Sha256::digest(normalized_dir.as_bytes());
    let digest_hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    let skill_root_rel = format!("{SKILLS_CWD_ALIAS}/{folder}-{}", &digest_hex[..10]);
    let example_file = referenced_files.first().cloned();
    let relative_guidance = match &example_file {
        Some(file) => format!(
            "> below references side files such as `{file}`, prefer the\n> relative form rooted at the first path above — e.g. open `{skill_root_rel}/{file}`."
        ),
        None => "> below references side files, prefer the relative form rooted at the\n> first path above.".to_string(),
    };
    let absolute_guidance = match &example_file {
        Some(file) => format!(
            "> back to the absolute path: `{}`.",
            dir.join(file).to_string_lossy()
        ),
        None => "> back to the absolute skill root above.".to_string(),
    };
    let mut lines = vec![
        format!("> **Skill root (relative to project):** `{skill_root_rel}/`"),
        format!("> **Skill root (absolute fallback):** `{}`", dir.to_string_lossy()),
        ">".to_string(),
        "> This skill ships side files alongside `SKILL.md`. When the workflow".to_string(),
        relative_guidance,
        "> If that path is not reachable from your working directory, fall".to_string(),
        absolute_guidance,
        "> Either form resolves to the same file; the relative form keeps you".to_string(),
        "> inside the project working directory, which is preferred.".to_string(),
    ];
    if !referenced_files.is_empty() {
        let listed = referenced_files
            .iter()
            .map(|file| format!("`{file}`"))
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(">".to_string());
        lines.push(format!("> Known side files in this skill: {listed}."));
    }
    lines.push(String::new());
    lines.push(String::new());
    format!("{}{}", lines.join("\n"), body)
}

/// Parity: `collectReferencedSideFiles` (`skills.ts:603`).
fn collect_referenced_side_files(body: &str) -> Vec<String> {
    static SIDE_FILE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static EXAMPLE_FILE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let side_file = SIDE_FILE.get_or_init(|| {
        Regex::new(r"\b(?:assets|references)/[A-Za-z0-9._-]+\b").expect("side file regex")
    });
    let example_file = EXAMPLE_FILE
        .get_or_init(|| Regex::new(r"\bexample\.html\b").expect("example file regex"));
    let mut files: HashSet<String> = HashSet::new();
    for found in side_file.find_iter(body) {
        files.insert(found.as_str().to_string());
    }
    if example_file.is_match(body) {
        files.insert("example.html".to_string());
    }
    let mut out: Vec<String> = files.into_iter().collect();
    out.sort();
    out
}

// ---- normalizers -----------------------------------------------------------

/// Parity: `normalizeCraftRequires` (`skills.ts:630`).
fn normalize_craft_requires(value: Option<&Value>) -> Vec<String> {
    let Some(items) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for item in items {
        let Some(text) = item.as_str() else {
            continue;
        };
        let slug = text.trim().to_lowercase();
        if slug.is_empty() || !is_slug(&slug) {
            continue;
        }
        if seen.insert(slug.clone()) {
            out.push(slug);
        }
    }
    out
}

fn is_slug(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
}

/// Parity: `normalizeDefaultFor` (`skills.ts:645`).
fn normalize_default_for(value: Option<&Value>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    if !js_truthy(value) {
        return Vec::new();
    }
    match value {
        Value::Array(items) => items.iter().map(js_string).collect(),
        other => vec![js_string(other)],
    }
}

fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().map(|n| n != 0.0 && !n.is_nan()).unwrap_or(false),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn js_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Bool(true) => "true".to_string(),
        Value::Bool(false) => "false".to_string(),
        Value::Null => "null".to_string(),
        Value::Number(number) => number.to_string(),
        Value::Array(_) => "[object Array]".to_string(),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

/// Parity: `normalizeFidelity` (`skills.ts:654`).
fn normalize_fidelity(value: Option<&Value>) -> Option<String> {
    match value.and_then(Value::as_str) {
        Some("wireframe") => Some("wireframe".to_string()),
        Some("high-fidelity") => Some("high-fidelity".to_string()),
        _ => None,
    }
}

/// Parity: `normalizeBoolHint` (`skills.ts:662`).
fn normalize_bool_hint(value: Option<&Value>) -> Option<bool> {
    match value {
        Some(Value::Bool(flag)) => Some(*flag),
        Some(Value::String(text)) => {
            let lowered = text.trim().to_lowercase();
            match lowered.as_str() {
                "true" | "yes" | "1" => Some(true),
                "false" | "no" | "0" => Some(false),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Parity: `localizedMapFromFields` (`skills.ts:672`).
fn localized_map_from_fields(en: Option<&Value>, zh: Option<&Value>) -> Option<Value> {
    let mut object = Map::new();
    if let Some(text) = en.and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty()) {
        object.insert("en".to_string(), Value::String(text.to_string()));
    }
    if let Some(text) = zh.and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty()) {
        object.insert("zh-CN".to_string(), Value::String(text.to_string()));
    }
    if object.is_empty() {
        None
    } else {
        Some(Value::Object(object))
    }
}

/// Parity: `localizedMapFromRecord` (`skills.ts:682`).
fn localized_map_from_record(value: Option<&Value>) -> Option<Value> {
    let object = value.and_then(Value::as_object)?;
    let mut out = Map::new();
    for (key, raw) in object {
        let Some(text) = raw.as_str() else {
            continue;
        };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        out.insert(key.clone(), Value::String(trimmed.to_string()));
    }
    if out.is_empty() {
        None
    } else {
        Some(Value::Object(out))
    }
}

/// Parity: `normalizeCritiquePolicy` (`skills.ts:706`).
fn normalize_critique_policy(value: Option<&Value>) -> Option<String> {
    let text = value.and_then(Value::as_str)?;
    match text.trim().to_lowercase().as_str() {
        "required" => Some("required".to_string()),
        "opt-in" => Some("opt-in".to_string()),
        "opt-out" => Some("opt-out".to_string()),
        _ => None,
    }
}

/// Parity: `normalizeFeatured` (`skills.ts:717`).
fn normalize_featured(value: Option<&Value>) -> Option<f64> {
    match value {
        Some(Value::Bool(true)) => Some(1.0),
        Some(Value::Number(number)) => number.as_f64().filter(|n| n.is_finite()),
        Some(Value::String(text)) if !text.trim().is_empty() => {
            text.trim().parse::<f64>().ok().filter(|n| n.is_finite())
        }
        _ => None,
    }
}

/// Parity: `derivePrompt` (`skills.ts:731`).
fn derive_prompt(data: &Map<String, Value>, od: Option<&Map<String, Value>>) -> String {
    if let Some(explicit) = od
        .and_then(|od| od.get("example_prompt"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        return explicit.to_string();
    }
    let description = data
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if description.is_empty() {
        return String::new();
    }
    static COLLAPSED: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static SENTENCE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let collapsed_re = COLLAPSED.get_or_init(|| Regex::new(r"\s+").expect("collapse regex"));
    let sentence_re = SENTENCE
        .get_or_init(|| Regex::new(r"^.+?[.!?。！？](?:\s|$)").expect("sentence regex"));
    let collapsed = collapsed_re.replace_all(description, " ").trim().to_string();
    let first_sentence = sentence_re
        .find(&collapsed)
        .map(|found| found.as_str().trim().to_string())
        .unwrap_or_else(|| collapsed.clone());
    first_sentence.chars().take(320).collect()
}

/// Parity: `inferMode` (`skills.ts:742`).
fn infer_mode(body: &str, description: &str) -> &'static str {
    static IMAGE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static VIDEO: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static AUDIO: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static DECK: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static DESIGN_SYSTEM: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static TEMPLATE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let image = IMAGE.get_or_init(|| {
        Regex::new(r"\bimage|poster|illustration|photography|图片|海报|插画").expect("image")
    });
    let video = VIDEO.get_or_init(|| {
        Regex::new(r"\bvideo|motion|shortform|animation|视频|动效|短片").expect("video")
    });
    let audio = AUDIO.get_or_init(|| {
        Regex::new(r"\baudio|music|jingle|tts|sound|音频|音乐|配音|音效").expect("audio")
    });
    let deck = DECK.get_or_init(|| {
        Regex::new(r"\bppt|deck|slide|presentation|幻灯|投影").expect("deck")
    });
    let design_system = DESIGN_SYSTEM.get_or_init(|| {
        Regex::new(r"\bdesign[- ]system|\bdesign\.md|\bdesign tokens").expect("design system")
    });
    let template =
        TEMPLATE.get_or_init(|| Regex::new(r"\btemplate\b").expect("template"));
    let hay = format!("{description}\n{body}").to_lowercase();
    if image.is_match(&hay) {
        return "image";
    }
    if video.is_match(&hay) {
        return "video";
    }
    if audio.is_match(&hay) {
        return "audio";
    }
    if deck.is_match(&hay) {
        return "deck";
    }
    if design_system.is_match(&hay) {
        return "design-system";
    }
    if template.is_match(&hay) {
        return "template";
    }
    "prototype"
}

/// Parity: `normalizeMode` (`skills.ts:754`).
fn normalize_mode(value: Option<&Value>, body: &str, description: &str) -> String {
    if let Some(text) = value.and_then(Value::as_str) {
        if matches!(
            text,
            "image" | "video" | "audio" | "deck" | "design-system" | "template" | "prototype"
        ) {
            return text.to_string();
        }
    }
    infer_mode(body, description).to_string()
}

/// Parity: `normalizeSurface` (`skills.ts:763`).
fn normalize_surface(value: Option<&Value>, mode: &str) -> String {
    if let Some(text) = value.and_then(Value::as_str) {
        let lowered = text.trim().to_lowercase();
        if matches!(lowered.as_str(), "web" | "image" | "video" | "audio") {
            return lowered;
        }
    }
    if matches!(mode, "image" | "video" | "audio") {
        return mode.to_string();
    }
    "web".to_string()
}

/// Parity: `normalizePlatform` (`skills.ts:775`).
fn normalize_platform(
    value: Option<&Value>,
    mode: &str,
    body: &str,
    description: &str,
) -> Option<String> {
    if let Some(text) = value.and_then(Value::as_str) {
        if matches!(text, "desktop" | "mobile") {
            return Some(text.to_string());
        }
    }
    if mode != "prototype" {
        return None;
    }
    static MOBILE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let mobile = MOBILE
        .get_or_init(|| Regex::new(r"mobile|phone|ios|android|手机|移动端").expect("mobile"));
    let hay = format!("{description}\n{body}").to_lowercase();
    if mobile.is_match(&hay) {
        Some("mobile".to_string())
    } else {
        Some("desktop".to_string())
    }
}

/// Parity: `normalizeCategory` (`skills.ts:808`).
fn normalize_category(value: Option<&Value>) -> Option<String> {
    let text = value.and_then(Value::as_str)?;
    let slug = text.trim().to_lowercase();
    if slug.is_empty() || !is_slug(&slug) {
        return None;
    }
    Some(slug.chars().take(64).collect())
}

/// Parity: `normalizeScenario` (`skills.ts:816`).
fn normalize_scenario(value: Option<&Value>, body: &str, description: &str) -> String {
    if let Some(text) = value.and_then(Value::as_str) {
        let lowered = text.trim().to_lowercase();
        if !lowered.is_empty() {
            return lowered;
        }
    }
    static COMPILED: std::sync::OnceLock<Vec<(Regex, &'static str)>> = std::sync::OnceLock::new();
    let compiled = COMPILED.get_or_init(|| {
        SCENARIO_HINTS
            .iter()
            .map(|(pattern, scenario)| (Regex::new(pattern).expect("scenario regex"), *scenario))
            .collect()
    });
    let hay = format!("{description}\n{body}").to_lowercase();
    for (regex, scenario) in compiled {
        if regex.is_match(&hay) {
            return (*scenario).to_string();
        }
    }
    "general".to_string()
}

/// The scenario heuristics in their TypeScript order (`skills.ts:830-841`).
const SCENARIO_HINTS: &[(&str, &str)] = &[
    (r"finance|invoice|expense|budget|p&l|revenue", "finance"),
    (r"\bhr\b|onboarding|payroll|employee|人事", "hr"),
    (r"marketing|campaign|brand|landing", "marketing"),
    (r"runbook|incident|deploy|engineering|sre|api", "engineering"),
    (r"spec|prd|roadmap|product manager|product team", "product"),
    (r"design system|moodboard|mockup|ui kit", "design"),
    (r"sales|quote|proposal|lead", "sales"),
    (r"operations|ops|logistics|inventory", "operations"),
];

/// Convenience for tests and callers that want the catalog as JSON.
pub fn catalog_json(skills: &[SkillInfo]) -> Value {
    json!({
        "skills": skills.iter().map(SkillInfo::summary_json).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, folder: &str, contents: &str) {
        let dir = root.join(folder);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("SKILL.md"), contents).expect("write");
    }

    #[test]
    fn listing_derives_the_web_contract_fields() {
        let root = std::env::temp_dir().join(format!("od-catalog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("mkdir");
        write_skill(
            &root,
            "hero",
            "---\nname: hero\ndescription: Build a hero section for a landing page.\nod:\n  mode: prototype\n  featured: true\n  craft:\n    requires:\n      - typography\n      - Typography\ntriggers:\n  - hero\n---\nbody\n",
        );
        let skills = list_skills(std::slice::from_ref(&root));
        assert_eq!(skills.len(), 1);
        let skill = &skills[0];
        assert_eq!(skill.id, "hero");
        assert_eq!(skill.mode, "prototype");
        assert_eq!(skill.platform.as_deref(), Some("desktop"));
        assert_eq!(skill.scenario, "marketing");
        assert_eq!(skill.featured, Some(1.0));
        assert_eq!(skill.craft_requires, vec!["typography".to_string()]);
        assert_eq!(skill.example_prompt, "Build a hero section for a landing page.");
        assert!(!skill.aggregates_examples);
        let summary = skill.summary_json();
        assert_eq!(summary["hasBody"], Value::Bool(true));
        assert!(summary.get("body").is_none());
        assert!(summary.get("dir").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn derived_examples_become_their_own_cards() {
        let root = std::env::temp_dir().join(format!("od-catalog-derived-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("mkdir");
        write_skill(&root, "gallery", "---\nname: gallery\ndescription: Samples.\n---\nbody\n");
        let examples = root.join("gallery").join("examples");
        std::fs::create_dir_all(&examples).expect("mkdir");
        std::fs::write(examples.join("stock-portfolio.html"), "<html></html>").expect("write");
        std::fs::write(examples.join("notes.txt"), "ignored").expect("write");
        let skills = list_skills(std::slice::from_ref(&root));
        assert_eq!(skills.len(), 2);
        assert!(skills[0].aggregates_examples);
        assert_eq!(skills[1].id, "gallery:stock-portfolio");
        assert_eq!(skills[1].name, "Stock Portfolio");
        assert!(!skills[1].aggregates_examples);
        // The `examples/` folder is a side attachment, so the parent body
        // carries the staged-root preamble (parity: `dirHasAttachments`).
        assert!(skills[1].body.contains("Skill root (relative to project)"));
        assert!(skills[1].body.ends_with("body\n"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_first_root_wins_a_duplicate_id() {
        let user = std::env::temp_dir().join(format!("od-catalog-user-{}", std::process::id()));
        let builtin = std::env::temp_dir().join(format!("od-catalog-builtin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&user);
        let _ = std::fs::remove_dir_all(&builtin);
        write_skill(&user, "shadow", "---\nname: shared\ndescription: user\n---\n");
        write_skill(&builtin, "original", "---\nname: shared\ndescription: built-in\n---\n");
        let skills = list_skills(&[user.clone(), builtin.clone()]);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "user");
        assert_eq!(skills[0].source, "user");
        let _ = std::fs::remove_dir_all(&user);
        let _ = std::fs::remove_dir_all(&builtin);
    }
}
