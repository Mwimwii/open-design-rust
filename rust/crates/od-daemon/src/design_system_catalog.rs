//! Design-system catalog — parity port of `listDesignSystems`
//! (`apps/daemon/src/design-systems/index.ts:319`), the two root policies
//! `listAllDesignSystems` applies (`server-services.ts:360`), and the
//! swatch/title/summary extraction that feeds the picker.
//!
//! Two roots exist, and they are not interchangeable:
//!
//! * the built-in root (`OD_RESOURCE_ROOT/design-systems`) is shipped with the
//!   app: `source: 'built-in'`, read-only, always `published`;
//! * the user root (`<data>/design-systems`) holds systems authored locally:
//!   `id` is prefixed `user:`, `source: 'user'`, editable, `draft` by default.
//!
//! The catalogue order is the user systems (newest `updatedAt` first) followed
//! by the built-in systems, matching `listAllDesignSystems`.
//!
//! The headerless lane has no workspace identity, so the scoped read is the
//! "explicitly empty scope" case (`workspaceId: null`): an unclaimed local
//! system stays visible, a system whose `metadata.json` claims a workspace is
//! hidden rather than trusted without authority.
//!
//! DOCUMENTED DEVIATIONS
//!
//! * Form D of `extractSwatches` — SwiftUI `Color(...)` declarations parsed by
//!   `swift-colors.ts` — is not ported. No shipped DESIGN.md uses it; a
//!   DESIGN.md that quotes Swift source yields no swatches from those lines
//!   instead of converting HSB/RGB values.
//! * `summarize` and `cleanTitle` slice/count UTF-16 code units in
//!   TypeScript; the port counts `char`s. They differ only for astral-plane
//!   characters.
//! * `metadata.json` `provenance` is normalized with the same field rules, but
//!   the port has no revision/team subsystems, so `teamSynced`/`workspaceId`
//!   are read for visibility only.
//! * The detail route reports `canMutate` from `isEditable`; the TypeScript
//!   route additionally checks team-share authority, which this daemon does
//!   not have.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::frontmatter::parse_frontmatter;

/// One catalogue entry, matching the TypeScript `DesignSystemSummary` shape.
#[derive(Clone, Debug)]
pub struct DesignSystemSummary {
    pub id: String,
    pub title: String,
    pub category: String,
    pub summary: String,
    pub swatches: Vec<String>,
    pub surface: String,
    pub body: String,
    pub source: String,
    pub status: String,
    pub is_editable: bool,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub provenance: Option<Value>,
    pub project_id: Option<String>,
    pub team_synced: bool,
    pub workspace_id: Option<String>,
}

impl DesignSystemSummary {
    /// The summary object; `body` is included only for the detail route
    /// (the listing strips it, parity: `static-resource.ts:860`).
    pub fn to_json(&self, include_body: bool) -> Value {
        let mut object = Map::new();
        object.insert("id".to_string(), Value::String(self.id.clone()));
        object.insert("title".to_string(), Value::String(self.title.clone()));
        object.insert("category".to_string(), Value::String(self.category.clone()));
        object.insert("summary".to_string(), Value::String(self.summary.clone()));
        object.insert(
            "swatches".to_string(),
            Value::Array(self.swatches.iter().cloned().map(Value::String).collect()),
        );
        object.insert("surface".to_string(), Value::String(self.surface.clone()));
        if include_body {
            object.insert("body".to_string(), Value::String(self.body.clone()));
        }
        object.insert("source".to_string(), Value::String(self.source.clone()));
        object.insert("status".to_string(), Value::String(self.status.clone()));
        object.insert("isEditable".to_string(), Value::Bool(self.is_editable));
        if let Some(created_at) = &self.created_at {
            object.insert("createdAt".to_string(), Value::String(created_at.clone()));
        }
        if let Some(updated_at) = &self.updated_at {
            object.insert("updatedAt".to_string(), Value::String(updated_at.clone()));
        }
        if let Some(provenance) = &self.provenance {
            object.insert("provenance".to_string(), provenance.clone());
        }
        if let Some(project_id) = &self.project_id {
            object.insert("projectId".to_string(), Value::String(project_id.clone()));
        }
        if self.team_synced {
            object.insert("teamSynced".to_string(), Value::Bool(true));
        }
        if let Some(workspace_id) = &self.workspace_id {
            object.insert(
                "workspaceId".to_string(),
                Value::String(workspace_id.clone()),
            );
        }
        Value::Object(object)
    }
}

/// The per-root policy `listAllDesignSystems` applies.
pub struct ListOptions<'a> {
    pub id_prefix: Option<&'a str>,
    pub source: &'a str,
    pub is_editable: bool,
    pub default_status: &'a str,
}

/// Parity: `listDesignSystems` (`index.ts:319`).
pub fn list_design_systems(root: &Path, options: &ListOptions) -> Vec<DesignSystemSummary> {
    let Ok(read_dir) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut entries: Vec<_> = read_dir.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    let mut out = Vec::new();
    for entry in entries {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() && !file_type.is_symlink() {
            continue;
        }
        let dir_id = entry.file_name().to_string_lossy().into_owned();
        let brand_root = entry.path();
        let manifest = read_project_manifest(&brand_root, &dir_id);
        let design_name = manifest
            .as_ref()
            .map(|manifest| manifest.design.clone())
            .unwrap_or_else(|| "DESIGN.md".to_string());
        let design_path = brand_root.join(design_name);
        if !design_path.is_file() {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&design_path) else {
            continue;
        };
        let metadata = read_user_metadata(root, &dir_id);
        let parsed = parse_frontmatter(&raw);
        let body = parsed.body;
        let data = parsed.data;

        let markdown_title = title_regex()
            .captures(&body)
            .and_then(|captures| captures.get(1))
            .map(|found| clean_title(found.as_str()))
            .unwrap_or_default();
        let fallback_title = if !markdown_title.is_empty() {
            markdown_title
        } else {
            let frontmatter_name = string_field(&data, "name");
            if !frontmatter_name.is_empty() {
                frontmatter_name
            } else {
                dir_id.clone()
            }
        };
        let title = clean_title(
            metadata
                .title
                .as_deref()
                .or(manifest.as_ref().map(|manifest| manifest.name.as_str()))
                .unwrap_or(&fallback_title),
        );
        let frontmatter_category = string_field(&data, "category");
        let category = metadata
            .category
            .clone()
            .or_else(|| manifest.as_ref().map(|manifest| manifest.category.clone()))
            .or_else(|| extract_category(&body))
            .or_else(|| {
                if frontmatter_category.is_empty() {
                    None
                } else {
                    Some(frontmatter_category.clone())
                }
            })
            .unwrap_or_else(|| "Uncategorized".to_string());
        let markdown_summary = summarize(&body);
        let markdown_swatches = extract_swatches(&body);
        let frontmatter_swatches = swatches_from_frontmatter(&data);
        let swatches = pick_final_swatch_row(frontmatter_swatches, &markdown_swatches);
        let summary = manifest
            .as_ref()
            .and_then(|manifest| manifest.description.as_deref())
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
            .or({
                if markdown_summary.is_empty() {
                    None
                } else {
                    Some(markdown_summary)
                }
            })
            .or_else(|| {
                let description = string_field(&data, "description");
                if description.is_empty() {
                    None
                } else {
                    Some(description)
                }
            })
            .unwrap_or_default();
        let surface = metadata
            .surface
            .clone()
            .or_else(|| extract_surface(&body))
            .or_else(|| frontmatter_surface(&data))
            .unwrap_or_else(|| "web".to_string());
        out.push(DesignSystemSummary {
            id: format!("{}{}", options.id_prefix.unwrap_or(""), dir_id),
            title,
            category,
            summary,
            swatches,
            surface,
            body: raw,
            source: options.source.to_string(),
            status: metadata
                .status
                .clone()
                .unwrap_or_else(|| options.default_status.to_string()),
            is_editable: options.is_editable,
            created_at: metadata.created_at,
            updated_at: metadata.updated_at,
            provenance: metadata.provenance,
            project_id: metadata.project_id,
            team_synced: metadata.team_synced,
            workspace_id: metadata.workspace_id,
        });
    }
    out
}

/// Parity: `listAllDesignSystems` for the headerless lane (`workspaceId:
/// null`): user systems newest-first, then built-in systems, with
/// workspace-claimed user systems hidden (fail-closed without authority).
pub fn list_all_design_systems(user_root: &Path, builtin_root: Option<&Path>) -> Vec<DesignSystemSummary> {
    let builtin = builtin_root
        .map(|root| {
            list_design_systems(
                root,
                &ListOptions {
                    id_prefix: None,
                    source: "built-in",
                    is_editable: false,
                    default_status: "published",
                },
            )
        })
        .unwrap_or_default();
    let mut user = list_design_systems(
        user_root,
        &ListOptions {
            id_prefix: Some("user:"),
            source: "user",
            is_editable: true,
            default_status: "draft",
        },
    );
    user.retain(|system| system.workspace_id.is_none());
    user.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    let mut catalog = user;
    catalog.extend(builtin);
    catalog
}

/// Parity: `findDesignSystem` over an already-listed catalogue. The `user:`
/// prefix is part of the id the listing produced, so ids compare verbatim.
pub fn find_design_system<'a>(
    catalog: &'a [DesignSystemSummary],
    id: &str,
) -> Option<&'a DesignSystemSummary> {
    if id.is_empty() {
        return None;
    }
    catalog.iter().find(|system| system.id == id)
}

// ---- manifest / metadata ---------------------------------------------------

struct ProjectManifest {
    name: String,
    category: String,
    description: Option<String>,
    design: String,
}

/// Parity: `readProjectManifest` + `isProjectManifest` (`index.ts:4079`).
fn read_project_manifest(brand_root: &Path, expected_id: &str) -> Option<ProjectManifest> {
    let raw = std::fs::read_to_string(brand_root.join("manifest.json")).ok()?;
    let parsed: Value = serde_json::from_str(&raw).ok()?;
    let object = parsed.as_object()?;
    if object.get("schemaVersion").and_then(Value::as_str) != Some("od-design-system-project/v1") {
        return None;
    }
    if object.get("id").and_then(Value::as_str) != Some(expected_id) {
        return None;
    }
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())?
        .to_string();
    let category = object
        .get("category")
        .and_then(Value::as_str)
        .filter(|category| !category.trim().is_empty())?
        .to_string();
    let description = match object.get("description") {
        None => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => return None,
    };
    let files = object.get("files")?.as_object()?;
    if files.get("design").and_then(Value::as_str) != Some("DESIGN.md") {
        return None;
    }
    if files.get("tokens").and_then(Value::as_str) != Some("tokens.css") {
        return None;
    }
    for (key, expected) in [
        ("designTokens", "design-tokens.json"),
        ("tailwind", "tailwind-v4.css"),
        ("components", "components.html"),
    ] {
        match files.get(key) {
            None => {}
            Some(Value::String(text)) if text == expected => {}
            Some(_) => return None,
        }
    }
    Some(ProjectManifest {
        name,
        category,
        description,
        design: "DESIGN.md".to_string(),
    })
}

#[derive(Default)]
struct UserMetadata {
    title: Option<String>,
    category: Option<String>,
    surface: Option<String>,
    status: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
    provenance: Option<Value>,
    project_id: Option<String>,
    team_synced: bool,
    workspace_id: Option<String>,
}

/// Parity: `readUserMetadata` (`index.ts:2942`).
fn read_user_metadata(root: &Path, id: &str) -> UserMetadata {
    let Ok(raw) = std::fs::read_to_string(root.join(id).join("metadata.json")) else {
        return UserMetadata::default();
    };
    let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
        return UserMetadata::default();
    };
    let Some(object) = parsed.as_object() else {
        return UserMetadata::default();
    };
    let string = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    UserMetadata {
        title: string("title"),
        category: string("category"),
        surface: string("surface").filter(|value| is_surface(value)),
        status: string("status").filter(|value| is_status(value)),
        created_at: string("createdAt"),
        updated_at: string("updatedAt"),
        provenance: parse_provenance(object.get("provenance")),
        project_id: clean_project_id_for_metadata(object.get("projectId")),
        team_synced: object.get("teamSynced") == Some(&Value::Bool(true)),
        workspace_id: clean_workspace_id_for_metadata(object.get("workspaceId")),
    }
}

/// Parity: `cleanWorkspaceIdForMetadata` (`index.ts:2971`).
fn clean_workspace_id_for_metadata(raw: Option<&Value>) -> Option<String> {
    let value = raw?.as_str()?.trim();
    if value.is_empty() || !is_opaque_id(value) {
        return None;
    }
    Some(value.to_string())
}

/// Parity: `cleanProjectIdForMetadata` (`index.ts:2980`).
fn clean_project_id_for_metadata(raw: Option<&Value>) -> Option<String> {
    let value = raw?.as_str()?.trim();
    if value.is_empty() || value == "." || value == ".." || !is_opaque_id(value) {
        return None;
    }
    Some(value.to_string())
}

/// Parity: `/^[A-Za-z0-9._:-]{1,160}$/`.
fn is_opaque_id(value: &str) -> bool {
    !value.is_empty()
        && value.chars().count() <= 160
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | ':' | '-'))
}

/// Parity: `parseProvenance` + `normalizeProvenance` (`index.ts:3344`).
fn parse_provenance(raw: Option<&Value>) -> Option<Value> {
    let object = raw?.as_object()?;
    let string = |key: &str| object.get(key).and_then(Value::as_str);
    let list = |key: &str| unique_clean_list(object.get(key).and_then(Value::as_array));
    let company_blurb = clean_multiline(string("companyBlurb"));
    let source_urls = list("sourceUrls");
    let github_urls = list("githubUrls");
    let local_code_files = list("localCodeFiles");
    let fig_files = list("figFiles");
    let asset_files = list("assetFiles");
    let notes = clean_multiline(string("notes"));
    let source_notes = clean_multiline(string("sourceNotes"));
    let mut out = Map::new();
    if !company_blurb.is_empty() {
        out.insert("companyBlurb".to_string(), Value::String(company_blurb));
    }
    for (key, values) in [
        ("sourceUrls", source_urls),
        ("githubUrls", github_urls),
        ("localCodeFiles", local_code_files),
        ("figFiles", fig_files),
        ("assetFiles", asset_files),
    ] {
        if !values.is_empty() {
            out.insert(
                key.to_string(),
                Value::Array(values.into_iter().map(Value::String).collect()),
            );
        }
    }
    if !notes.is_empty() {
        out.insert("notes".to_string(), Value::String(notes));
    }
    if !source_notes.is_empty() {
        out.insert("sourceNotes".to_string(), Value::String(source_notes));
    }
    if out.is_empty() {
        None
    } else {
        Some(Value::Object(out))
    }
}

/// Parity: `cleanText` (`index.ts:3329`).
fn clean_text(raw: &str) -> String {
    whitespace_regex().replace_all(raw.trim(), " ").trim().to_string()
}

/// Parity: `cleanMultiline` (`index.ts:3333`).
fn clean_multiline(raw: Option<&str>) -> String {
    let Some(raw) = raw else {
        return String::new();
    };
    let normalized = raw.replace("\r\n", "\n");
    let lines: Vec<String> = normalized
        .split('\n')
        .map(|line| {
            let trimmed = line.trim();
            let collapsed = horizontal_space_regex().replace_all(trimmed, " ");
            collapsed.into_owned()
        })
        .collect();
    let joined = lines.join("\n");
    let collapsed = triple_newline_regex().replace_all(&joined, "\n\n");
    collapsed.trim().to_string()
}

/// Parity: `uniqueCleanList` (`index.ts:3395`), capped at 100 entries.
fn unique_clean_list(values: Option<&Vec<Value>>) -> Vec<String> {
    let Some(values) = values else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for value in values {
        let Some(text) = value.as_str() else {
            continue;
        };
        let clean = clean_text(text);
        if clean.is_empty() || !seen.insert(clean.clone()) {
            continue;
        }
        out.push(clean);
        if out.len() >= 100 {
            break;
        }
    }
    out
}

// ---- markdown extraction ---------------------------------------------------

fn string_field(data: &Map<String, Value>, key: &str) -> String {
    data.get(key)
        .and_then(Value::as_str)
        .map(|value| value.trim().to_string())
        .unwrap_or_default()
}

fn frontmatter_surface(data: &Map<String, Value>) -> Option<String> {
    let value = string_field(data, "surface").to_lowercase();
    if is_surface(&value) {
        Some(value)
    } else {
        None
    }
}

fn is_surface(value: &str) -> bool {
    matches!(value, "web" | "image" | "video" | "audio")
}

fn is_status(value: &str) -> bool {
    matches!(value, "draft" | "published")
}

/// Parity: `extractCategory` (`index.ts:4137`).
fn extract_category(raw: &str) -> Option<String> {
    category_regex()
        .captures(raw)
        .and_then(|captures| captures.get(1))
        .map(|found| found.as_str().to_string())
}

/// Parity: `extractSurface` (`index.ts:4149`).
fn extract_surface(raw: &str) -> Option<String> {
    let found = surface_regex()
        .captures(raw)
        .and_then(|captures| captures.get(1))
        .map(|found| found.as_str().trim().to_lowercase())?;
    if is_surface(&found) {
        Some(found)
    } else {
        None
    }
}

/// Parity: `cleanTitle` (`index.ts:4173`).
fn clean_title(raw: &str) -> String {
    title_prefix_regex().replace(raw, "").trim().to_string()
}

/// Parity: `summarize` (`index.ts:4121`).
fn summarize(raw: &str) -> String {
    let lines: Vec<&str> = raw.split('\n').map(|line| line.trim_end_matches('\r')).collect();
    let Some(first_h1) = lines.iter().position(|line| line.starts_with("# ")) else {
        return String::new();
    };
    let after_h1 = &lines[first_h1 + 1..];
    let next_heading = after_h1
        .iter()
        .position(|line| heading_regex().is_match(line));
    let window_lines = match next_heading {
        Some(index) => &after_h1[..index],
        None => after_h1,
    };
    let joined = window_lines.join("\n");
    let without_category = category_line_regex().replace_all(&joined, "");
    let without_surface = surface_line_regex().replace_all(&without_category, "");
    let without_quotes = blockquote_prefix_regex().replace_all(&without_surface, "");
    let trimmed = without_quotes.trim();
    trimmed
        .split("\n\n")
        .next()
        .map(|first| first.chars().take(240).collect())
        .unwrap_or_default()
}

struct ColorToken {
    name: String,
    value: String,
}

/// Parity: `extractSwatches` (`index.ts:4192`), forms A/B/C.
fn extract_swatches(raw: &str) -> Vec<String> {
    let mut colors: Vec<ColorToken> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let push = |name: &str, value: &str, colors: &mut Vec<ColorToken>, seen: &mut HashSet<String>| {
        let clean_name = swatch_name_noise_regex().replace_all(name, "");
        let clean_name = whitespace_regex().replace_all(clean_name.trim(), " ").to_lowercase();
        let Some(hex) = normalize_hex(value) else {
            return;
        };
        if clean_name.chars().count() > 60 {
            return;
        }
        let key = format!("{clean_name}|{hex}");
        if !seen.insert(key) {
            return;
        }
        colors.push(ColorToken {
            name: clean_name,
            value: hex,
        });
    };

    for captures in swatch_form_a_regex().captures_iter(raw) {
        push(
            captures.get(1).map(|found| found.as_str()).unwrap_or(""),
            captures.get(2).map(|found| found.as_str()).unwrap_or(""),
            &mut colors,
            &mut seen,
        );
    }
    for captures in swatch_form_b_regex().captures_iter(raw) {
        push(
            captures.get(1).map(|found| found.as_str()).unwrap_or(""),
            captures.get(2).map(|found| found.as_str()).unwrap_or(""),
            &mut colors,
            &mut seen,
        );
    }
    for captures in swatch_table_row_regex().captures_iter(raw) {
        let row = captures.get(1).map(|found| found.as_str()).unwrap_or("");
        let cells: Vec<&str> = row.split('|').map(str::trim).collect();
        let Some(hex_cell) = cells.iter().find(|cell| hex_in_cell_regex().is_match(cell)) else {
            continue;
        };
        let hex = hex_in_cell_regex()
            .find(hex_cell)
            .map(|found| found.as_str())
            .unwrap_or("");
        let name_cell = cells.iter().find(|cell| {
            !cell.is_empty()
                && !hex_in_cell_regex().is_match(cell)
                && !table_separator_regex().is_match(cell)
        });
        push(
            name_cell.copied().unwrap_or(""),
            hex,
            &mut colors,
            &mut seen,
        );
    }
    // Form D (SwiftUI `Color(...)`) is deliberately not ported; see the module
    // docs. Inline hex forms above are what the shipped DESIGN.md files use.
    if colors.is_empty() {
        return Vec::new();
    }
    pick_swatch_row(&colors).0
}

/// Parity: `swatchesFromFrontmatter` (`index.ts:447`).
fn swatches_from_frontmatter(data: &Map<String, Value>) -> Option<(Vec<String>, bool)> {
    let raw = data.get("colors")?.as_object()?;
    let mut colors: Vec<ColorToken> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (name, value) in raw {
        let Some(value) = value.as_str() else {
            continue;
        };
        let Some(hex) = normalize_hex(value) else {
            continue;
        };
        let clean_name = whitespace_regex()
            .replace_all(name.trim(), " ")
            .to_lowercase();
        let key = format!("{clean_name}|{hex}");
        if !seen.insert(key) {
            continue;
        }
        colors.push(ColorToken {
            name: clean_name,
            value: hex,
        });
    }
    if colors.is_empty() {
        return None;
    }
    Some(pick_swatch_row(&colors))
}

/// Parity: `pickFinalSwatchRow` (`index.ts:466`).
fn pick_final_swatch_row(
    frontmatter: Option<(Vec<String>, bool)>,
    markdown_swatches: &[String],
) -> Vec<String> {
    if let Some((values, true)) = &frontmatter {
        return values.clone();
    }
    if !markdown_swatches.is_empty() {
        return markdown_swatches.to_vec();
    }
    frontmatter.map(|(values, _)| values).unwrap_or_default()
}

/// Parity: `pickSwatchRow` (`index.ts:4243`). Returns the four swatches and
/// whether every slot was named explicitly (the `filledAllSlots` flag).
fn pick_swatch_row(colors: &[ColorToken]) -> (Vec<String>, bool) {
    let pick = |hints: &[&str]| -> Option<String> {
        for hint in hints {
            if let Some(found) = colors.iter().find(|color| color.name.contains(hint)) {
                return Some(found.value.clone());
            }
        }
        None
    };
    let bg_hit = pick(&["page background", "background", "canvas", "paper", "surface"]);
    let fg_hit = pick(&["heading", "foreground", "ink", "fg", "text", "navy", "graphite"]);
    let accent_hit = pick(&["primary brand", "brand primary", "accent", "brand", "primary"]);
    let support_hit = pick(&["border", "divider", "rule", "muted", "secondary", "subtle"]);

    let bg = bg_hit.clone().unwrap_or_else(|| "#ffffff".to_string());
    let fg = fg_hit.clone().unwrap_or_else(|| "#111111".to_string());
    let accent = accent_hit
        .clone()
        .or_else(|| {
            colors
                .iter()
                .find(|color| !is_neutral(&color.value))
                .map(|color| color.value.clone())
        })
        .or_else(|| colors.first().map(|color| color.value.clone()))
        .unwrap_or_else(|| "#888888".to_string());
    let support = support_hit
        .clone()
        .or_else(|| {
            colors
                .iter()
                .find(|color| {
                    is_neutral(&color.value) && color.value != bg && color.value != fg
                })
                .map(|color| color.value.clone())
        })
        .unwrap_or_else(|| "#cccccc".to_string());
    let filled_all_slots = bg_hit.is_some()
        && fg_hit.is_some()
        && accent_hit.is_some()
        && support_hit.is_some();
    (vec![bg, support, fg, accent], filled_all_slots)
}

/// Parity: `isNeutral` (`index.ts:4256`).
fn is_neutral(hex: &str) -> bool {
    if hex.chars().count() != 7 || !hex.starts_with('#') {
        return false;
    }
    let channel = |range: std::ops::Range<usize>| -> Option<i32> {
        i32::from_str_radix(&hex[range], 16).ok()
    };
    let (Some(r), Some(g), Some(b)) = (channel(1..3), channel(3..5), channel(5..7)) else {
        return false;
    };
    r.max(g).max(b) - r.min(g).min(b) < 10
}

/// Parity: `normalizeHex` (`index.ts:4290`).
fn normalize_hex(raw: &str) -> Option<String> {
    let captured = hex_only_regex().captures(raw.trim())?;
    let mut hex = captured.get(1)?.as_str().to_string();
    if hex.len() == 3 {
        hex = hex.chars().flat_map(|ch| [ch, ch]).collect();
    }
    if hex.len() == 4 {
        let doubled: String = hex.chars().flat_map(|ch| [ch, ch]).collect();
        hex = doubled.chars().take(8).collect();
    }
    Some(format!("#{}", hex.to_lowercase()))
}

// ---- compiled patterns -----------------------------------------------------

fn title_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^#\s+(.+?)\s*$").expect("title regex"))
}

fn heading_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^#{1,6}\s+").expect("heading regex"))
}

fn category_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?im)^>\s*Category:\s*(.+?)\s*$").expect("category regex"))
}

fn surface_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?im)^>\s*Surface:\s*(.+?)\s*$").expect("surface regex"))
}

fn category_line_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?im)^>\s*Category:.*$").expect("category line regex"))
}

fn surface_line_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?im)^>\s*Surface:.*$").expect("surface line regex"))
}

fn blockquote_prefix_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^>\s*").expect("blockquote prefix regex"))
}

fn title_prefix_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^Design System (Inspired by|for)\s+").expect("title prefix"))
}

fn whitespace_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s+").expect("whitespace regex"))
}

fn horizontal_space_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[ \t]+").expect("horizontal space regex"))
}

fn triple_newline_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\n{3,}").expect("triple newline regex"))
}

fn swatch_name_noise_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[*_`]+").expect("swatch noise regex"))
}

fn swatch_form_a_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?m)^[\s>*-]*([A-Za-z][A-Za-z0-9 /&()+_-]{1,40}?)\s*[:：]?\s*\**\s*[:：]?\s*`?(#[0-9a-fA-F]{3,8})",
        )
        .expect("swatch form A")
    })
}

fn swatch_form_b_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\*\*([A-Za-z][A-Za-z0-9 /&()+_-]{1,40}?)\*\*\s*\(?\s*`?(#[0-9a-fA-F]{3,8})")
            .expect("swatch form B")
    })
}

fn swatch_table_row_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^[ \t]*\|(.+)\|[ \t]*$").expect("swatch table row"))
}

fn hex_in_cell_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"#[0-9a-fA-F]{3,8}\b").expect("hex in cell"))
}

fn table_separator_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[-:\s]+$").expect("table separator"))
}

fn hex_only_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^#([0-9a-fA-F]{3,8})$").expect("hex only"))
}

/// Resolve the two catalogue roots from the daemon data directory.
pub fn design_system_roots(data_dir: &Path) -> (PathBuf, Option<PathBuf>) {
    let user_root = data_dir.join("design-systems");
    let builtin_root = crate::static_resources::resource_root().map(|root| root.join("design-systems"));
    (user_root, builtin_root)
}

/// Convenience for callers that only need JSON.
pub fn catalog_json(catalog: &[DesignSystemSummary]) -> Value {
    json!({
        "designSystems": catalog
            .iter()
            .map(|system| system.to_json(false))
            .collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_system(root: &Path, folder: &str, design: &str) {
        let dir = root.join(folder);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("DESIGN.md"), design).expect("write");
    }

    #[test]
    fn listing_reads_title_category_summary_and_swatches() {
        let root = std::env::temp_dir().join(format!("od-ds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("mkdir");
        write_system(
            &root,
            "cohere",
            "# Design System Inspired by Cohere\n\n> Category: AI\n> Surface: web\n\nWarm editorial AI brand with a calm grid.\n\n## Colors\n\n- **Background:** `#FAFAFA`\n- **Heading:** `#1A1A1A`\n- **Primary:** `#39594D`\n- **Border:** `#D9D9D9`\n",
        );
        let catalog = list_design_systems(
            &root,
            &ListOptions {
                id_prefix: Some("user:"),
                source: "user",
                is_editable: true,
                default_status: "draft",
            },
        );
        assert_eq!(catalog.len(), 1);
        let system = &catalog[0];
        assert_eq!(system.id, "user:cohere");
        assert_eq!(system.title, "Cohere");
        assert_eq!(system.category, "AI");
        assert_eq!(system.surface, "web");
        assert_eq!(
            system.summary,
            "Warm editorial AI brand with a calm grid."
        );
        assert_eq!(
            system.swatches,
            vec![
                "#fafafa".to_string(),
                "#d9d9d9".to_string(),
                "#1a1a1a".to_string(),
                "#39594d".to_string()
            ]
        );
        assert_eq!(system.status, "draft");
        assert!(system.is_editable);
        assert!(system.body.contains("Warm editorial"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_manifest_can_rename_the_folder_and_supply_metadata() {
        let root = std::env::temp_dir().join(format!("od-ds-manifest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("brand");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("manifest.json"),
            r#"{"schemaVersion":"od-design-system-project/v1","id":"brand","name":"Acme Brand","category":"Brand","description":"A crisp industrial identity.","files":{"design":"DESIGN.md","tokens":"tokens.css"}}"#,
        )
        .expect("write");
        std::fs::write(
            dir.join("DESIGN.md"),
            "# Some Title\n\nBody copy.\n",
        )
        .expect("write");
        std::fs::write(
            dir.join("metadata.json"),
            r#"{"title":"Renamed","category":"Editorial","status":"published","updatedAt":"2026-02-01T00:00:00Z"}"#,
        )
        .expect("write");
        let catalog = list_design_systems(
            &root,
            &ListOptions {
                id_prefix: None,
                source: "built-in",
                is_editable: false,
                default_status: "published",
            },
        );
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].title, "Renamed");
        assert_eq!(catalog[0].category, "Editorial");
        assert_eq!(catalog[0].summary, "A crisp industrial identity.");
        assert_eq!(catalog[0].status, "published");
        assert_eq!(
            catalog[0].updated_at.as_deref(),
            Some("2026-02-01T00:00:00Z")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_workspace_claim_hides_the_system_without_authority() {
        let root = std::env::temp_dir().join(format!("od-ds-claim-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        write_system(&root, "local", "# Local\n\nBody.\n");
        write_system(&root, "claimed", "# Claimed\n\nBody.\n");
        std::fs::write(
            root.join("claimed").join("metadata.json"),
            r#"{"workspaceId":"ws_123"}"#,
        )
        .expect("write");
        let catalog = list_all_design_systems(&root, None);
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].id, "user:local");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn user_systems_come_before_builtins_newest_first() {
        let user = std::env::temp_dir().join(format!("od-ds-user-{}", std::process::id()));
        let builtin = std::env::temp_dir().join(format!("od-ds-builtin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&user);
        let _ = std::fs::remove_dir_all(&builtin);
        write_system(&user, "older", "# Older\n\nBody.\n");
        write_system(&user, "newer", "# Newer\n\nBody.\n");
        std::fs::write(
            user.join("older").join("metadata.json"),
            r#"{"updatedAt":"2026-01-01T00:00:00Z"}"#,
        )
        .expect("write");
        std::fs::write(
            user.join("newer").join("metadata.json"),
            r#"{"updatedAt":"2026-03-01T00:00:00Z"}"#,
        )
        .expect("write");
        write_system(&builtin, "shipped", "# Shipped\n\nBody.\n");
        let catalog = list_all_design_systems(&user, Some(&builtin));
        let ids: Vec<&str> = catalog.iter().map(|system| system.id.as_str()).collect();
        assert_eq!(ids, vec!["user:newer", "user:older", "shipped"]);
        let _ = std::fs::remove_dir_all(&user);
        let _ = std::fs::remove_dir_all(&builtin);
    }

    #[test]
    fn hex_normalization_expands_short_forms() {
        assert_eq!(normalize_hex("#abc").as_deref(), Some("#aabbcc"));
        assert_eq!(normalize_hex("#ABCD").as_deref(), Some("#aabbccdd"));
        assert_eq!(normalize_hex(" not-a-hex"), None);
    }
}
