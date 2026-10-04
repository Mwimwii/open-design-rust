//! Static skill / design-template resource routes — parity port of the two
//! static file routes in `apps/daemon/src/routes/static-resource.ts`:
//!
//! * `GET /api/skills/:id/example` (static-resource.ts:922)
//! * `GET /api/skills/:id/assets/*splat` (static-resource.ts:1059)
//!
//! plus the helpers they call: `assembleExample` (static-resource.ts:1498),
//! `rewriteSkillAssetUrls` (static-resource.ts:1515), the catalogue scan
//! behind `listAllSkillLikeEntries` (design-systems/server-services.ts:323 →
//! `listSkills` in skills.ts:232), the id-alias table and derived-id helpers
//! (skills.ts:28 / 488 / 514 / 522), and a port of `findRealElementRange`
//! (`packages/contracts/src/runtime/html-injection-points.ts:862`) for the
//! `<title>` retitle in `assembleExample`.
//!
//! Catalogue CRUD routes from `static-resource.ts` (skill install/import,
//! atoms, design-systems, prompt/design templates, codex-pets) are out of
//! scope: they need subsystems the Rust daemon does not have yet.
//!
//! DOCUMENTED DEVIATIONS
//!
//! * TypeScript answers these routes' error cases with `text/plain` bodies;
//!   the Rust daemon answers every error with the JSON error envelope
//!   `{"error":{"code","message"}}`. Status codes and message strings are
//!   preserved verbatim (`skill not found`, `derived example not found`,
//!   `invalid asset path`, `asset not found`, and the final "no example…"
//!   404). Parity source for the envelope convention: `tests/files.rs`.
//! * Workspace authority (`resolveWorkspaceAuthority`) is not ported — the
//!   Rust daemon runs the headerless lane, authority is always `null`, so
//!   `workspaceQuery` is always the empty string (exactly what TypeScript
//!   sends when no workspace headers are present). The
//!   navigation-query-vs-header conflict check and `workspace_resources`
//!   visibility filtering are absent (no workspace tables in this daemon).
//! * The bundled `PROJECT_ROOT` fallback roots (the `fallback` argument to
//!   `resolveDaemonResourceDir`) do not exist here — there is no project
//!   root. Roots are `<data>/skills`, `<data>/design-templates`, plus
//!   `OD_RESOURCE_ROOT/{skills,design-templates}` when that env var is set,
//!   in the functional-then-template order `listAllSkillLikeEntries` uses
//!   for a headerless request (`server-services.ts:323`).
//! * Frontmatter parsing is a subset of `parseFrontmatter`
//!   (design-systems/frontmatter.ts:17): only a `name:` key decides the id,
//!   matched at the root of the YAML stack. Block scalars (`name: |`) fall
//!   back to the folder name instead of becoming a multi-line id.
//! * `find_real_title_range` ports `findRealElementRange` but does not model
//!   the `<select>` insertion mode (`observeSelectMode`): indeterminate
//!   in-select transitions refuse the lookup in TypeScript, and `<svg>`
//!   inside `<select>` is walked as HTML there. `skipForeignContent` is a
//!   name-depth approximation (no namespace stack / integration points, and
//!   stray `</p>`/`</br>` always refuse instead of only when unmatched).
//! * Nested symlinks inside a skill folder are refused by `resolve_under`
//!   where TypeScript's textual `path.resolve` + `startsWith` would follow
//!   them out of the tree (same stance the project `/raw` routes take).
//! * Express `sendFile`'s ETag / Range / 304 handling on the asset route is
//!   not ported; assets are answered with plain bytes, like `/raw`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use axum::extract::{Path as PathExtractor, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::project_dir::{map_io, resolve_under, ProjectPathError};
use crate::project_files::mime_for;
use crate::routes::{api_error, internal_error, AppState};

/// Parity: the two static routes of `registerStaticResourceRoutes`
/// (`server.ts:8983` mounts the router; every CRUD route is out of scope).
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/skills/{id}/example", get(skill_example))
        .route("/api/skills/{id}/assets/{*splat}", get(skill_asset))
}

// ---- routes ----------------------------------------------------------------

async fn skill_example(State(state): State<AppState>, PathExtractor(id): PathExtractor<String>) -> Response {
    let data_dir = state.config.paths.data_dir().to_path_buf();
    match tokio::task::spawn_blocking(move || example_response(&data_dir, &id)).await {
        Ok(response) => response,
        Err(err) => internal_error(&err.to_string()),
    }
}

async fn skill_asset(
    State(state): State<AppState>,
    PathExtractor((id, splat)): PathExtractor<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let origin_null = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        == Some("null");
    let data_dir = state.config.paths.data_dir().to_path_buf();
    match tokio::task::spawn_blocking(move || asset_response(&data_dir, &id, &splat, origin_null)).await
    {
        Ok(response) => response,
        Err(err) => internal_error(&err.to_string()),
    }
}

/// Parity: the `/example` route body (static-resource.ts:922-1057).
fn example_response(data_dir: &Path, id: &str) -> Response {
    let entries = list_skill_like_entries(data_dir);

    // 1. Derived `<parent>:<child>` id — resolved straight to the file under
    //    `<parentDir>/examples/`, BEFORE `findSkillById`, so a missing sample
    //    404s explicitly instead of falling into the parent's fallback chain.
    if let Some(derived) = split_derived_skill_id(id) {
        let Some(parent) = find_skill_by_id(&entries, derived.parent_id) else {
            return api_error(StatusCode::NOT_FOUND, "NOT_FOUND", "skill not found");
        };
        let rel = format!("examples/{}.html", derived.child_key);
        return match resolve_under(&parent.dir, &rel) {
            Ok(resolved) => match std::fs::read(&resolved.path) {
                Ok(bytes) => html_response(rewrite_skill_asset_urls(
                    &String::from_utf8_lossy(&bytes),
                    &parent.id,
                    "",
                )),
                // Parity: an existing-but-unreadable file hits the route's
                // outer catch → 500 `String(err)`.
                Err(err) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", &map_io(err).to_string()),
            },
            Err(ProjectPathError::NotFound) | Err(ProjectPathError::Escape) => {
                api_error(StatusCode::NOT_FOUND, "NOT_FOUND", "derived example not found")
            }
            Err(err) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", &err.to_string()),
        };
    }

    let Some(skill) = find_skill_by_id(&entries, id) else {
        return api_error(StatusCode::NOT_FOUND, "NOT_FOUND", "skill not found");
    };

    // 2. Fully-baked static example (preferred).
    match read_example_step(&skill.dir, "example.html") {
        Step::Content(html) => {
            return html_response(rewrite_skill_asset_urls(&html, &skill.id, ""));
        }
        Step::Missing => {}
        Step::Failed(message) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", &message),
    }

    // 3. Seed template + slides fragment, assembled at request time. Any read
    //    failure falls through to the raw template, exactly like the route's
    //    inner try/catch.
    if let (Step::Content(tpl), Step::Content(slides)) = (
        read_example_step(&skill.dir, "assets/template.html"),
        read_example_step(&skill.dir, "assets/example-slides.html"),
    ) {
        let assembled = assemble_example(&tpl, &slides, &skill.id);
        return html_response(rewrite_skill_asset_urls(&assembled, &skill.id, ""));
    }

    // 4. Raw template, no content slides.
    match read_example_step(&skill.dir, "assets/template.html") {
        Step::Content(html) => {
            return html_response(rewrite_skill_asset_urls(&html, &skill.id, ""));
        }
        Step::Missing => {}
        Step::Failed(message) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", &message),
    }

    // 5. Generic fallback.
    match read_example_step(&skill.dir, "assets/index.html") {
        Step::Content(html) => {
            return html_response(rewrite_skill_asset_urls(&html, &skill.id, ""));
        }
        Step::Missing => {}
        Step::Failed(message) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", &message),
    }

    // 6. First `.html` in `examples/` — a friendly fallback for skills that
    //    aggregate samples beside SKILL.md (e.g. live-artifact).
    if let Some(html) = first_examples_html(&skill.dir, &skill.id) {
        return html_response(html);
    }

    api_error(
        StatusCode::NOT_FOUND,
        "NOT_FOUND",
        "no example.html, assets/template.html, assets/index.html, or examples/*.html for this skill",
    )
}

/// One step of the example resolution chain: `Missing` mirrors TypeScript's
/// `fs.existsSync(...) === false` (including symlink escapes, which this
/// daemon refuses rather than reads — see the module docs).
enum Step {
    Content(String),
    Missing,
    /// Read/IO failure after a successful resolve — the route's outer catch
    /// answers 500 `String(err)` for these.
    Failed(String),
}

fn read_example_step(base: &Path, rel: &str) -> Step {
    match resolve_under(base, rel) {
        Ok(resolved) => match std::fs::read(&resolved.path) {
            Ok(bytes) => Step::Content(String::from_utf8_lossy(&bytes).into_owned()),
            Err(err) => Step::Failed(map_io(err).to_string()),
        },
        Err(ProjectPathError::NotFound) | Err(ProjectPathError::Escape) => Step::Missing,
        Err(err) => Step::Failed(err.to_string()),
    }
}

/// Parity: the `examples/` fallback loop — sorted names, dotfiles skipped,
/// read failures continue to the next candidate.
fn first_examples_html(skill_dir: &Path, skill_id: &str) -> Option<String> {
    let examples = resolve_under(skill_dir, "examples").ok()?.path;
    let mut names: Vec<String> = std::fs::read_dir(&examples)
        .ok()?
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    // Parity: `entries.sort()` — ASCII names order identically under byte and
    // UTF-16 comparison; non-ASCII ordering may differ (documented).
    names.sort();
    for name in names {
        if name.starts_with('.') {
            continue;
        }
        if !name.to_lowercase().ends_with(".html") {
            continue;
        }
        let rel = format!("examples/{name}");
        if let Step::Content(html) = read_example_step(skill_dir, &rel) {
            return Some(rewrite_skill_asset_urls(&html, skill_id, ""));
        }
    }
    None
}

/// Parity: the `/assets/*splat` route body (static-resource.ts:1059-1113).
fn asset_response(data_dir: &Path, id: &str, rel_path: &str, origin_null: bool) -> Response {
    let entries = list_skill_like_entries(data_dir);
    let Some(skill) = find_skill_by_id(&entries, id) else {
        return api_error(StatusCode::NOT_FOUND, "NOT_FOUND", "skill not found");
    };

    // Parity: `fs.existsSync` reports false for a path containing NUL.
    if rel_path.contains('\0') {
        return api_error(StatusCode::NOT_FOUND, "NOT_FOUND", "asset not found");
    }

    // Parity: `path.resolve(assetsRoot, relPath)` + the textual containment
    // check — this decides `invalid asset path` BEFORE any existence check,
    // so a traversal attempt 404s never.
    let assets_root = skill.dir.join("assets");
    let normalized_rel = match lexical_resolve_under(&assets_root, rel_path) {
        Ok(rel) => rel,
        Err(()) => return api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", "invalid asset path"),
    };

    let resolved = match resolve_under(&assets_root, &normalized_rel) {
        Ok(resolved) => resolved,
        Err(ProjectPathError::NotFound) => {
            return api_error(StatusCode::NOT_FOUND, "NOT_FOUND", "asset not found");
        }
        Err(ProjectPathError::Escape) => {
            // TypeScript would follow the escaping symlink; this daemon
            // refuses it (documented deviation).
            return api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", "invalid asset path");
        }
        Err(err) => {
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", &err.to_string());
        }
    };

    let bytes = match std::fs::read(&resolved.path) {
        Ok(bytes) => bytes,
        // Directories and raced-away files hit Express' `sendFile` error
        // handler → 500 `String(err)`.
        Err(err) => {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                &map_io(err).to_string(),
            );
        }
    };

    let mime = mime_for(&resolved.path.to_string_lossy());
    let mut response = (
        StatusCode::OK,
        [(header::CONTENT_TYPE, mime)],
        bytes,
    )
        .into_response();
    // The example HTML is rendered in a sandboxed iframe (`Origin: null`) and
    // must be able to fetch its own image bytes (parity: same allowance as
    // the project `/raw` route).
    if origin_null {
        response.headers_mut().insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        );
    }
    response
}

fn html_response(body: String) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}

// ---- skill catalogue -------------------------------------------------------

/// One on-disk skill or design-template folder. `id` is the frontmatter
/// `name` (when it is a non-empty string) or the folder name — the same
/// `parentId` TypeScript's `listSkills` derives, which it also surfaces as
/// `name` (`name: parentId`), so `skill.name === skill.id` here.
struct SkillEntry {
    id: String,
    dir: PathBuf,
}

/// Parity: `listAllSkillLikeEntries({ workspaceId: null, … })` for a
/// headerless request (server-services.ts:323): functional roots first, then
/// template roots minus the ids functional already claimed.
fn list_skill_like_entries(data_dir: &Path) -> Vec<SkillEntry> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    scan_roots(&roots_for(data_dir, "skills"), &mut seen, &mut out);
    // The same `seen` set applies TypeScript's
    // `templates.filter((t) => !functionalIds.has(t.id))` for free.
    scan_roots(&roots_for(data_dir, "design-templates"), &mut seen, &mut out);
    out
}

/// `<data>/<segment>` first, then `OD_RESOURCE_ROOT/<segment>` when set
/// (parity: `ALL_SKILL_LIKE_ROOTS` with the `resolveDaemonResourceDir`
/// fallbacks reduced to the data-dir pair — see the module docs).
fn roots_for(data_dir: &Path, segment: &str) -> Vec<PathBuf> {
    let mut roots = vec![data_dir.join(segment)];
    if let Some(resource_root) = resource_root() {
        roots.push(resource_root.join(segment));
    }
    roots
}

/// Parity: `resolveDaemonResourceRoot` reading `OD_RESOURCE_ROOT`
/// (daemon-paths.ts:76). The TypeScript safe-base validation has no Rust
/// counterpart (no project root exists here), and a relative value resolves
/// against the current directory like `path.resolve`.
fn resource_root() -> Option<PathBuf> {
    let raw = std::env::var("OD_RESOURCE_ROOT").ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let path = Path::new(trimmed);
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        Some(std::env::current_dir().ok()?.join(path))
    }
}

/// Parity: the per-root scan of `listSkills` (skills.ts:232) — child folders
/// (or symlinks) containing a readable `SKILL.md`; the first root to surface
/// an id wins.
fn scan_roots(roots: &[PathBuf], seen: &mut HashSet<String>, out: &mut Vec<SkillEntry>) {
    for root in roots {
        let Ok(read_dir) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in read_dir.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() && !file_type.is_symlink() {
                continue;
            }
            let dir = entry.path();
            let skill_path = dir.join("SKILL.md");
            let Ok(metadata) = std::fs::metadata(&skill_path) else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let Ok(raw) = std::fs::read(&skill_path) else {
                continue;
            };
            let raw = String::from_utf8_lossy(&raw);
            let folder = entry.file_name().to_string_lossy().into_owned();
            let id = match frontmatter_name(&raw) {
                Some(name) if !name.is_empty() => name,
                _ => folder,
            };
            if !seen.insert(id.clone()) {
                continue;
            }
            out.push(SkillEntry { id, dir });
        }
    }
}

/// `SKILL_ID_ALIASES` (skills.ts:28) — persisted ids can outlive a folder
/// rename, so lookup ids are forwarded to their canonical form first.
fn resolve_skill_id(id: &str) -> &str {
    match id {
        "editorial-collage" => "open-design-landing",
        "editorial-collage-deck" => "open-design-landing-deck",
        "taste-skill" => "design-taste-frontend",
        other => other,
    }
}

/// Parity: `findSkillById` (skills.ts:148).
fn find_skill_by_id<'a>(entries: &'a [SkillEntry], id: &str) -> Option<&'a SkillEntry> {
    if id.is_empty() {
        return None;
    }
    let canonical = resolve_skill_id(id);
    entries.iter().find(|entry| entry.id == canonical)
}

/// Minimal frontmatter `name:` extraction — the `parentId` rule of
/// `listSkills` (skills.ts:302) over a subset of `parseFrontmatter`
/// (design-systems/frontmatter.ts:17).
///
/// The YAML stack of the full parser is reduced to container indents, which
/// is exactly what decides whether a `name:` key lands on the root object:
/// blank and comment lines are skipped, `key:` with no value opens a nested
/// container, sequence items never change the stack for root purposes, and
/// the LAST root-level `name:` wins (later assignments overwrite, so a final
/// empty-valued `name:` leaves an object → folder name).
///
/// DEVIATION: block scalars (`name: |`) are not collected — they fall back
/// to the folder name instead of becoming a multi-line id.
fn frontmatter_name(raw: &str) -> Option<String> {
    let text = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let mut lines = text.split('\n').map(strip_cr);
    if lines.next()? != "---" {
        return None;
    }
    let body: Vec<&str> = lines.collect();
    let end = body.iter().position(|line| *line == "---")?;
    let yaml = &body[..end];

    // Root level is `-1`, mirroring the parser's initial stack entry.
    let mut stack: Vec<i32> = vec![-1];
    let mut name: Option<Option<String>> = None;
    let mut index = 0;
    while index < yaml.len() {
        let line = yaml[index];
        let indent = leading_indent(line);
        if line.is_empty() || line.trim_start().starts_with('#') {
            index += 1;
            continue;
        }
        while stack.len() > 1 && (indent as i32) <= *stack.last().expect("stack is non-empty") {
            stack.pop();
        }
        let at_root = stack.len() == 1;

        // Sequence items: the full parser may push object-item frames, but
        // they only nest deeper than their parent key, so skipping them keeps
        // the root/nested verdict identical for `name` detection.
        if line.starts_with("- ") {
            index += 1;
            continue;
        }

        let Some(colon) = line.find(':') else {
            index += 1;
            continue;
        };
        let key = line[..colon].trim();
        let value = line[colon + 1..].trim();

        if value.is_empty() {
            // `key:` with no value stores an object and opens a frame.
            if at_root && key == "name" {
                name = Some(None);
            }
            stack.push(indent as i32);
            index += 1;
            continue;
        }

        if matches!(value, "|" | "|-" | ">" | ">-") {
            // Block scalar: consume its content lines (documented deviation —
            // the block never becomes an id).
            if at_root && key == "name" {
                name = Some(None);
            }
            index += 1;
            while index < yaml.len() {
                let next = yaml[index];
                if next.is_empty() {
                    index += 1;
                    continue;
                }
                if leading_indent(next) <= indent {
                    break;
                }
                index += 1;
            }
            continue;
        }

        if at_root && key == "name" {
            name = Some(coerce_string(value));
        }
        index += 1;
    }

    name.flatten()
}

fn strip_cr(line: &str) -> &str {
    line.strip_suffix('\r').unwrap_or(line)
}

/// YAML indentation as the parser measures it: the length of the leading
/// whitespace run (JS `^\s*`), counted in characters.
fn leading_indent(line: &str) -> usize {
    line.chars()
        .take_while(|c| c.is_whitespace())
        .map(char::len_utf8)
        .count()
}

/// Parity: `coerce` (frontmatter.ts:196) restricted to the string case —
/// quoted scalars are unquoted; `true`/`false`/`null`/`~` and base-10
/// integers/floats are not strings and fall back to the folder name.
fn coerce_string(raw: &str) -> Option<String> {
    let value = raw.trim();
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        return Some(value[1..value.len() - 1].to_string());
    }
    if matches!(value, "true" | "false" | "null" | "~") {
        return None;
    }
    if is_js_integer(value) || is_js_float(value) {
        return None;
    }
    Some(value.to_string())
}

/// `/^-?\d+$/` — `\d` is ASCII-only in JavaScript.
fn is_js_integer(value: &str) -> bool {
    let digits = value.strip_prefix('-').unwrap_or(value);
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

/// `/^-?\d*\.\d+$/`.
fn is_js_float(value: &str) -> bool {
    let value = value.strip_prefix('-').unwrap_or(value);
    let Some(dot) = value.find('.') else {
        return false;
    };
    let (head, tail) = (&value[..dot], &value[dot + 1..]);
    !tail.is_empty()
        && tail.bytes().all(|byte| byte.is_ascii_digit())
        && head.bytes().all(|byte| byte.is_ascii_digit())
}

// ---- derived example ids ---------------------------------------------------

struct DerivedSkillId<'a> {
    parent_id: &'a str,
    child_key: &'a str,
}

/// Parity: `splitDerivedSkillId` (skills.ts:522). The FIRST `:` splits; a
/// leading/trailing colon or an unsafe child key yields `None` so the caller
/// falls back to the regular listing lookup.
fn split_derived_skill_id(id: &str) -> Option<DerivedSkillId<'_>> {
    let index = id.find(':')?;
    if index == 0 || index == id.len() - 1 {
        return None;
    }
    let child_key = &id[index + 1..];
    if !is_safe_example_key(child_key) {
        return None;
    }
    Some(DerivedSkillId {
        parent_id: &id[..index],
        child_key,
    })
}

/// Parity: `isSafeExampleKey` (skills.ts:488) — letters, digits, dash, dot,
/// underscore; never empty, never a dotfile prefix, never a colon.
fn is_safe_example_key(key: &str) -> bool {
    if key.is_empty() || key.starts_with('.') || key.contains(':') {
        return false;
    }
    key.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

// ---- asset path containment ------------------------------------------------

/// Parity: `path.resolve(root, rel)` plus the textual containment check of
/// the asset route (static-resource.ts:1075-1078), without touching the
/// filesystem: `Ok` carries the root-relative normalized path (`""` for the
/// root itself, which TypeScript allows by explicit equality), `Err(())` is
/// the `invalid asset path` verdict. Splitting the lexical decision from
/// `resolve_under` keeps "outside → 400" ahead of "outside → 404", which is
/// what makes an unseeded traversal attempt still answer 400.
fn lexical_resolve_under(root: &Path, rel: &str) -> Result<String, ()> {
    let root = absolutize(root)?;
    let base = absolute_components(&root)?;
    let mut stack = base.clone();
    let root_len = stack.len();
    if rel.starts_with('/') {
        stack = absolute_components(Path::new(rel))?;
    } else {
        for part in rel.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    if stack.len() == root_len {
                        return Err(());
                    }
                    stack.pop();
                }
                part => stack.push(part.to_string()),
            }
        }
    }
    if stack.len() < base.len() || stack[..base.len()] != base[..] {
        return Err(());
    }
    Ok(stack[base.len()..].join("/"))
}

fn absolutize(path: &Path) -> Result<PathBuf, ()> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir().map_err(|_| ())?.join(path))
    }
}

/// Absolute path → component list with `.`/`..` resolved (parity:
/// `path.resolve`), rooted at `/`.
fn absolute_components(path: &Path) -> Result<Vec<String>, ()> {
    let mut out = vec![String::new()];
    for part in path.to_string_lossy().split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if out.len() > 1 {
                    out.pop();
                }
            }
            part => out.push(part.to_string()),
        }
    }
    Ok(out)
}

// ---- rewriteSkillAssetUrls -------------------------------------------------

/// Parity: `rewriteSkillAssetUrls` (static-resource.ts:1515) — a hand scan of
/// `/(\s(?:src|href)\s*=\s*)(['"])((?:\.\.\/([^/'"#?]+)\/)?(?:\.\/)?assets\/([^'"#?]+))(\2)/gi`
/// so no regex crate is needed. Matches continue after each replacement, one
/// whitespace character anchors the attribute, `assets` matches
/// case-insensitively, and the sibling prefix (`../<id>/`) rewrites to that
/// id instead of this skill's.
fn rewrite_skill_asset_urls(html: &str, skill_id: &str, workspace_query: &str) -> String {
    if html.is_empty() {
        return html.to_string();
    }
    let mut matches: Vec<(usize, usize, String)> = Vec::new();
    let mut index = 0;
    while index < html.len() {
        // Char-boundary iteration: every candidate match starts on a JS
        // whitespace character, which may itself be multi-byte.
        let current = html[index..].chars().next().expect("index is on a char boundary");
        if !is_js_space(current) {
            index += current.len_utf8();
            continue;
        }
        if let Some((end, replacement)) =
            match_rewrite_at(html, index, skill_id, workspace_query)
        {
            matches.push((index, end, replacement));
            index = end;
        } else {
            index += current.len_utf8();
        }
    }
    if matches.is_empty() {
        return html.to_string();
    }
    let mut out = String::with_capacity(html.len() + matches.len() * 32);
    let mut copied = 0;
    for (start, end, replacement) in matches {
        out.push_str(&html[copied..start]);
        out.push_str(&replacement);
        copied = end;
    }
    out.push_str(&html[copied..]);
    out
}

/// Attempt one full match anchored at the whitespace at `start`. Returns the
/// byte offset just past the closing quote plus the replacement text.
fn match_rewrite_at(
    html: &str,
    start: usize,
    skill_id: &str,
    workspace_query: &str,
) -> Option<(usize, String)> {
    let bytes = html.as_bytes();
    let anchor_width = html[start..].chars().next()?.len_utf8();
    let attr_at = start + anchor_width;
    // JS `/i` on the attribute name (parity: `(?:src|href)` under the `i`
    // flag); `src` is tried first, like the alternation.
    let attr_len = if bytes
        .get(attr_at..attr_at + 3)
        .is_some_and(|name| name.eq_ignore_ascii_case(b"src"))
    {
        3
    } else if bytes
        .get(attr_at..attr_at + 4)
        .is_some_and(|name| name.eq_ignore_ascii_case(b"href"))
    {
        4
    } else {
        return None;
    };
    let mut cursor = attr_at + attr_len;
    cursor = skip_js_spaces(html, cursor);
    if bytes.get(cursor) != Some(&b'=') {
        return None;
    }
    cursor = skip_js_spaces(html, cursor + 1);
    let quote = *bytes.get(cursor)?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let quote_start = cursor;
    cursor += 1;

    // Optional `../<sibling>/` prefix first, then the branch with no prefix
    // — the regex makes the group optional, so "no sibling" is its own
    // attempt at the same cursor, not a failed match.
    let mut attempts: Vec<(&str, usize)> = Vec::with_capacity(2);
    if let Some(sibling) = parse_sibling_prefix(html, cursor) {
        attempts.push(sibling);
    }
    attempts.push(("", cursor));
    for (sibling_id, after_sibling) in attempts {
        let mut position = after_sibling;
        // `(?:\.\/)?` greedy — when `./` is present, the no-`./` branch can
        // never match `assets/` at the `.` position, so one attempt suffices.
        if bytes.get(position..position + 2) == Some(b"./") {
            position += 2;
        }
        if !bytes
            .get(position..position + 7)?
            .eq_ignore_ascii_case(b"assets/")
        {
            continue;
        }
        let rel_start = position + 7;
        let rel_end = rel_start + span_before_excluded(bytes, rel_start);
        if rel_end == rel_start {
            continue; // `[^'"#?]+` needs at least one character
        }
        if bytes.get(rel_end) != Some(&quote) {
            // Backtracking cannot help: the excluded set contains the quote,
            // so no shorter/longer rel can end on it.
            continue;
        }
        let rel = &html[rel_start..rel_end];
        let resolved = if sibling_id.is_empty() {
            skill_id
        } else {
            sibling_id
        };
        let replacement = format!(
            "{}{}{}{}{}{}",
            &html[start..quote_start],
            quote as char,
            "/api/skills/",
            uri_component(resolved),
            "/assets/",
            rel,
        );
        let replacement = format!("{replacement}{workspace_query}{}", quote as char);
        return Some((rel_end + 1, replacement));
    }
    None
}

/// `(\.\.\/([^/'"#?]+)\/)?` at `position` — the sibling id and the offset
/// just past its closing slash.
fn parse_sibling_prefix(html: &str, position: usize) -> Option<(&str, usize)> {
    let bytes = html.as_bytes();
    if bytes.get(position..position + 3)? != b"../" {
        return None;
    }
    let start = position + 3;
    let mut end = start;
    while end < bytes.len() && !matches!(bytes[end], b'/' | b'\'' | b'"' | b'#' | b'?') {
        end += 1;
    }
    if end == start || bytes.get(end) != Some(&b'/') {
        return None;
    }
    Some((&html[start..end], end + 1))
}

/// Length of `[^'"#?] +` starting at `at`.
fn span_before_excluded(bytes: &[u8], at: usize) -> usize {
    let mut length = 0;
    while at + length < bytes.len() && !matches!(bytes[at + length], b'\'' | b'"' | b'#' | b'?') {
        length += 1;
    }
    length
}

fn skip_js_spaces(html: &str, mut position: usize) -> usize {
    while position < html.len() {
        let current = html[position..].chars().next().expect("non-empty slice");
        if !is_js_space(current) {
            break;
        }
        position += current.len_utf8();
    }
    position
}

/// ECMAScript `\s` — WhiteSpace plus LineTerminator. Differs from
/// `char::is_whitespace` at U+FEFF (in) and U+0085 (out).
fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
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

/// `encodeURIComponent`.
fn uri_component(value: &str) -> String {
    const UNRESERVED: &[u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.!~*'()";
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if UNRESERVED.contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

// ---- assembleExample + the HTML boundary scanner ---------------------------

/// Parity: `assembleExample` (static-resource.ts:1498). `str::replacen`
/// matches the TypeScript function replacer: the marker is replaced
/// literally, with no `$`-substitution of skill-derived inputs.
fn assemble_example(template_html: &str, slides_html: &str, title: &str) -> String {
    let with_slides = template_html.replacen("<!-- SLIDES_HERE -->", slides_html, 1);
    match find_real_title_range(&with_slides) {
        Some((start, end)) => format!(
            "{}<title>{} | OpenDesign Example</title>{}",
            &with_slides[..start],
            title,
            &with_slides[end..]
        ),
        None => with_slides,
    }
}

const RAW_TEXT_ELEMENTS: &[&str] = &[
    "script",
    "style",
    "textarea",
    "title",
    "iframe",
    "noembed",
    "noframes",
    "noscript",
    "plaintext",
    "xmp",
];

const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "track",
    "wbr",
];

const FOREIGN_BREAKOUT_TAGS: &[&str] = &[
    "b", "big", "blockquote", "body", "br", "center", "code", "dd", "div", "dl", "dt", "em",
    "embed", "h1", "h2", "h3", "h4", "h5", "h6", "head", "hr", "i", "img", "li", "listing",
    "menu", "meta", "nobr", "ol", "p", "pre", "ruby", "s", "small", "span", "strong", "strike",
    "sub", "sup", "table", "tt", "u", "ul", "var",
];

/// Parity: `findRealElementRange(html, HTML_TAG_PATTERNS.titleOpen, 'title')`
/// (html-injection-points.ts:862) — the byte range of the first REAL
/// `<title …>…</title>` pair. "Real" means a boundary the HTML tokenizer
/// would honor: a `<title>` inside a comment, a script string, an attribute
/// value, a `<template>`, or an `<svg>` subtree is content, not markup
/// (nexu-io/open-design#7410).
fn find_real_title_range(html: &str) -> Option<(usize, usize)> {
    let start = find_real_tag_offset(html)?;
    // `endOfTag(html, start)` — `scanTag` is handed the `<` itself here, the
    // same call shape the contracts module uses.
    let open_end = scan_tag(html, start)?.end;
    let lower = ascii_lower_shadow(html);
    let content_end = find_raw_text_close(&lower, "title", open_end + 1)?;
    let close_end = scan_tag(html, content_end)?.end;
    Some((start, close_end + 1))
}

/// Parity: `findRealTagOffset` (html-injection-points.ts:680) specialized to
/// the sticky `titleOpen` pattern (`/<title(?=[\t\n\f\r >])/i`), which the
/// walk tests at every `<` before parsing it as a tag.
///
/// DEVIATION: `observeSelectMode` is not ported (see module docs).
fn find_real_tag_offset(html: &str) -> Option<usize> {
    let bytes = html.as_bytes();
    let lower = ascii_lower_shadow(html);
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'<' {
            index += 1;
            continue;
        }
        if starts_with(bytes, index, b"<!--") {
            index = end_of_comment(html, index)?;
            continue;
        }
        if starts_with(bytes, index, b"</")
            && !bytes
                .get(index + 2)
                .is_some_and(u8::is_ascii_alphabetic)
        {
            // End-tag-open on a non-letter is a bogus comment → next `>`.
            index = find_byte(bytes, b'>', index + 2)? + 1;
            continue;
        }
        if starts_with(bytes, index, b"<!") || starts_with(bytes, index, b"<?") {
            index = find_byte(bytes, b'>', index + 2)? + 1;
            continue;
        }
        // The sticky `titleOpen` pattern, tested before the tag is parsed.
        if title_open_matches(bytes, index) {
            return Some(index);
        }
        let Some((is_end_tag, name_start, name_end, after_name)) = parse_tag_open(bytes, index)
        else {
            // A `<` that starts no tag is ordinary text (`a < b`).
            index += 1;
            continue;
        };
        let scanned = scan_tag(html, after_name)?;
        let tag_end = scanned.end;
        let tag_name = ascii_lower_str(&html[name_start..name_end]);
        if !is_end_tag && RAW_TEXT_ELEMENTS.contains(&tag_name.as_str()) {
            index = find_raw_text_close(&lower, &tag_name, tag_end + 1)?;
            continue;
        }
        if !is_end_tag && (tag_name == "svg" || tag_name == "math") && !scanned.self_closing {
            index = skip_foreign_content(html, &lower, &tag_name, tag_end + 1)?;
            continue;
        }
        if !is_end_tag && tag_name == "template" {
            index = skip_template_content(html, &lower, tag_end + 1)?;
            continue;
        }
        index = tag_end + 1;
    }
    None
}

/// `/<title(?=[\t\n\f\r >])/i` evaluated stickily at `index`.
fn title_open_matches(bytes: &[u8], index: usize) -> bool {
    if !bytes.get(index + 6).is_some_and(|_| true) {
        return false;
    }
    if bytes.len() < index + 7 {
        return false;
    }
    if !bytes[index..index + 6].eq_ignore_ascii_case(b"<title") {
        return false;
    }
    matches!(
        bytes[index + 6],
        b'\t' | b'\n' | 12 | b'\r' | b' ' | b'>'
    )
}

/// `/<(\/?)([a-z][^\t\n\f\r \/>]*)/iy` at `index` → `(isEndTag, nameStart,
/// nameEnd, offsetJustPastName)`.
fn parse_tag_open(bytes: &[u8], index: usize) -> Option<(bool, usize, usize, usize)> {
    let mut cursor = index + 1;
    let is_end_tag = bytes.get(cursor) == Some(&b'/');
    if is_end_tag {
        cursor += 1;
    }
    let first = *bytes.get(cursor)?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    let name_start = cursor;
    cursor += 1;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if byte == b'\t' || byte == b'\n' || byte == 12 || byte == b'\r' || byte == b' ' || byte == b'/' || byte == b'>' {
            break;
        }
        cursor += 1;
    }
    Some((is_end_tag, name_start, cursor, cursor))
}

struct ScannedTag {
    /// Offset of the `>` that closes the tag.
    end: usize,
    self_closing: bool,
    /// Lowercased attribute names (the presentational-attribute check for
    /// `font` breakout is the only consumer).
    attrs: Vec<String>,
}

/// Parity: `scanTag` (html-injection-points.ts:110) — one walk of a start or
/// end tag in the tokenizer's own states, so a `>` inside a quoted attribute
/// value cannot end the tag early. `from` is either the offset just past the
/// tag name or the `<` itself (both call shapes exist in the TS module).
/// `None` is TypeScript's `end: -1` (unterminated tag).
fn scan_tag(html: &str, from: usize) -> Option<ScannedTag> {
    let bytes = html.as_bytes();
    let mut attrs: Vec<String> = Vec::new();
    let mut cursor = from;
    let mut self_closing = false;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if is_html_whitespace(byte) {
            self_closing = false;
            cursor += 1;
            continue;
        }
        if byte == b'/' {
            self_closing = true;
            cursor += 1;
            continue;
        }
        if byte == b'>' {
            return Some(ScannedTag {
                end: cursor,
                self_closing,
                attrs,
            });
        }
        // Attribute-name state (the solidus was noise unless `>` followed).
        self_closing = false;
        let name_start = cursor;
        if byte == b'=' {
            cursor += 1;
        }
        while cursor < bytes.len() {
            let byte = bytes[cursor];
            if is_html_whitespace(byte) || byte == b'/' || byte == b'=' || byte == b'>' {
                break;
            }
            cursor += 1;
        }
        let name = ascii_lower_str(&html[name_start..cursor]);

        // After-attribute-name state.
        while cursor < bytes.len() && is_html_whitespace(bytes[cursor]) {
            cursor += 1;
        }
        if cursor >= bytes.len() {
            return None;
        }
        let mut value = "";
        if bytes[cursor] == b'=' {
            // Before-attribute-value state.
            cursor += 1;
            while cursor < bytes.len() && is_html_whitespace(bytes[cursor]) {
                cursor += 1;
            }
            if cursor >= bytes.len() {
                return None;
            }
            let quote = bytes[cursor];
            if quote == b'"' || quote == b'\'' {
                let mut close = cursor + 1;
                loop {
                    if close >= bytes.len() {
                        return None;
                    }
                    if bytes[close] == quote {
                        break;
                    }
                    close += 1;
                }
                value = &html[cursor + 1..close];
                cursor = close + 1;
            } else if quote == b'>' {
                // Missing value; the tag ends here.
                if !name.is_empty() && !attrs.contains(&name) {
                    attrs.push(name);
                }
                return Some(ScannedTag {
                    end: cursor,
                    self_closing: false,
                    attrs,
                });
            } else {
                let value_start = cursor;
                while cursor < bytes.len() {
                    let byte = bytes[cursor];
                    if is_html_whitespace(byte) || byte == b'>' {
                        break;
                    }
                    cursor += 1;
                }
                value = &html[value_start..cursor];
            }
        }
        if !name.is_empty() && !attrs.iter().any(|existing| existing == &name) {
            attrs.push(name);
            let _ = value;
        }
    }
    None
}

/// HTML ASCII whitespace: TAB, LF, FF, CR, SPACE — and nothing else
/// (html-injection-points.ts:222).
fn is_html_whitespace(byte: u8) -> bool {
    matches!(byte, 9 | 10 | 12 | 13 | 32)
}

fn is_end_tag_boundary(byte: u8) -> bool {
    is_html_whitespace(byte) || byte == b'/' || byte == b'>'
}

/// Parity: `endOfComment` (html-injection-points.ts:71) — a comment closes on
/// a run of dashes followed by `>` or `!>`, and `<!-->` / `<!--->` are closed
/// at the start.
fn end_of_comment(html: &str, from: usize) -> Option<usize> {
    let bytes = html.as_bytes();
    let mut cursor = from + 4;
    if starts_with(bytes, cursor, b">") {
        return Some(cursor + 1);
    }
    if starts_with(bytes, cursor, b"->") {
        return Some(cursor + 2);
    }
    while cursor < bytes.len() {
        let mut dash = cursor;
        loop {
            if dash + 1 >= bytes.len() {
                return None;
            }
            if bytes[dash] == b'-' && bytes[dash + 1] == b'-' {
                break;
            }
            dash += 1;
        }
        let mut end = dash;
        while bytes.get(end) == Some(&b'-') {
            end += 1;
        }
        if starts_with(bytes, end, b">") {
            return Some(end + 1);
        }
        if starts_with(bytes, end, b"!>") {
            return Some(end + 2);
        }
        cursor = if end > dash { end } else { dash + 2 };
    }
    None
}

/// Parity: `findRawTextClose` (html-injection-points.ts:231) — an end tag
/// only closes when its name is followed by whitespace, `/`, or `>`; a longer
/// name that merely starts with it (`</script-template>`) is character data.
fn find_raw_text_close(lower: &str, tag: &str, from: usize) -> Option<usize> {
    if tag == "plaintext" {
        return None;
    }
    if tag == "script" {
        return find_script_close(lower, from);
    }
    let needle = format!("</{tag}");
    let mut search_from = from;
    while search_from <= lower.len() {
        let rest = lower.get(search_from..)?;
        let offset = rest.find(&needle)?;
        let position = search_from + offset;
        match lower.as_bytes().get(position + needle.len()) {
            Some(&byte) if is_end_tag_boundary(byte) => return Some(position),
            Some(_) => search_from = position + needle.len(),
            // Past end of input: `charCodeAt` is `NaN`, which fails the
            // boundary test, and the next search finds nothing.
            None => return None,
        }
    }
    None
}

/// Parity: `findScriptClose` (html-injection-points.ts:257) — script data has
/// escape states, so `<!--` / nested `<script` / `-->` must be tracked before
/// accepting a `</script`.
fn find_script_close(lower: &str, from: usize) -> Option<usize> {
    let bytes = lower.as_bytes();
    let mut cursor = from;
    let mut escaped = false;
    let mut double_escaped = false;
    while cursor < bytes.len() {
        if !escaped && starts_with(bytes, cursor, b"<!--") {
            escaped = true;
            cursor += 4;
            continue;
        }
        if escaped && starts_with(bytes, cursor, b"-->") {
            escaped = false;
            double_escaped = false;
            cursor += 3;
            continue;
        }
        if escaped
            && !double_escaped
            && starts_with(bytes, cursor, b"<script")
            && bytes.get(cursor + 7).is_some_and(|&byte| is_end_tag_boundary(byte))
        {
            double_escaped = true;
            cursor += 7;
            continue;
        }
        if starts_with(bytes, cursor, b"</script")
            && bytes.get(cursor + 8).is_some_and(|&byte| is_end_tag_boundary(byte))
        {
            if !double_escaped {
                return Some(cursor);
            }
            double_escaped = false;
            cursor += 8;
            continue;
        }
        cursor += 1;
    }
    None
}

/// Parity: `skipTemplateContent` (html-injection-points.ts:290) — template
/// content is inert, so a `</body>` (or `<title>`) inside one is not a
/// document boundary. DEVIATION: no insertion-mode stack; `<svg>` inside a
/// `<select>` inside the template is treated as foreign here.
fn skip_template_content(html: &str, lower: &str, from: usize) -> Option<usize> {
    let bytes = html.as_bytes();
    let mut depth: usize = 1;
    let mut cursor = from;
    while cursor < bytes.len() {
        if bytes[cursor] != b'<' {
            cursor += 1;
            continue;
        }
        if starts_with(bytes, cursor, b"<!--") {
            cursor = end_of_comment(html, cursor)?;
            continue;
        }
        if starts_with(bytes, cursor, b"</")
            && !bytes.get(cursor + 2).is_some_and(u8::is_ascii_alphabetic)
        {
            cursor = find_byte(bytes, b'>', cursor + 2)? + 1;
            continue;
        }
        if starts_with(bytes, cursor, b"<!") || starts_with(bytes, cursor, b"<?") {
            cursor = find_byte(bytes, b'>', cursor + 2)? + 1;
            continue;
        }
        let Some((is_end_tag, name_start, name_end, after_name)) =
            parse_tag_open(bytes, cursor)
        else {
            cursor += 1;
            continue;
        };
        let scanned = scan_tag(html, after_name)?;
        let tag_end = scanned.end;
        let tag_name = ascii_lower_str(&html[name_start..name_end]);
        if !is_end_tag && RAW_TEXT_ELEMENTS.contains(&tag_name.as_str()) {
            cursor = find_raw_text_close(lower, &tag_name, tag_end + 1)?;
            continue;
        }
        if !is_end_tag && (tag_name == "svg" || tag_name == "math") && !scanned.self_closing {
            cursor = skip_foreign_content(html, lower, &tag_name, tag_end + 1)?;
            continue;
        }
        if tag_name == "template" {
            if is_end_tag {
                depth -= 1;
                if depth == 0 {
                    return Some(tag_end + 1);
                }
            } else if !scanned.self_closing {
                depth += 1;
            }
        }
        cursor = tag_end + 1;
    }
    None
}

/// DEVIATION: name-depth approximation of `skipForeignContent`
/// (html-injection-points.ts:556). The TypeScript scan keeps a namespace
/// stack with integration points, `<![CDATA[…]]>` rules per namespace, and
/// breakout pops; this port counts open frames by the root element name and
/// refuses only where the TS scan refuses outright (stray `</p>` / `</br>`).
/// The subtree-skip verdict — no `<title>` inside `<svg>`/`<math>` is a
/// document boundary — is preserved.
fn skip_foreign_content(html: &str, lower: &str, root_name: &str, from: usize) -> Option<usize> {
    let bytes = html.as_bytes();
    let mut depth: usize = 1;
    let mut cursor = from;
    while cursor < bytes.len() {
        if bytes[cursor] != b'<' {
            cursor += 1;
            continue;
        }
        if starts_with(bytes, cursor, b"<!--") {
            cursor = end_of_comment(html, cursor)?;
            continue;
        }
        if starts_with(bytes, cursor, b"</")
            && !bytes.get(cursor + 2).is_some_and(u8::is_ascii_alphabetic)
        {
            cursor = find_byte(bytes, b'>', cursor + 2)? + 1;
            continue;
        }
        if starts_with(bytes, cursor, b"<![cdata[") {
            cursor = find_bytes(bytes, b"]]]>", cursor + 9)? + 3;
            continue;
        }
        if starts_with(bytes, cursor, b"<!") || starts_with(bytes, cursor, b"<?") {
            cursor = find_byte(bytes, b'>', cursor + 2)? + 1;
            continue;
        }
        let Some((is_end_tag, name_start, name_end, after_name)) =
            parse_tag_open(bytes, cursor)
        else {
            cursor += 1;
            continue;
        };
        let scanned = scan_tag(html, after_name)?;
        let tag_end = scanned.end;
        let tag_name = ascii_lower_str(&html[name_start..name_end]);
        if is_end_tag {
            if tag_name == root_name {
                depth -= 1;
                if depth == 0 {
                    return Some(tag_end + 1);
                }
            } else if tag_name == "p" || tag_name == "br" {
                // `REPROCESSED_END_TAGS`: TS refuses whenever no matching
                // frame is open; the depth model never tracks those frames.
                return None;
            }
            cursor = tag_end + 1;
            continue;
        }
        if is_foreign_breakout_tag(&tag_name, &scanned) {
            // TS pops every foreign frame and reprocesses this tag under HTML
            // rules — in the depth model that closes the subtree right here.
            return Some(cursor);
        }
        if RAW_TEXT_ELEMENTS.contains(&tag_name.as_str()) {
            cursor = find_raw_text_close(lower, &tag_name, tag_end + 1)?;
            continue;
        }
        if !scanned.self_closing
            && !VOID_ELEMENTS.contains(&tag_name.as_str())
            && tag_name == root_name
        {
            depth += 1;
        }
        cursor = tag_end + 1;
    }
    None
}

/// Parity: `isForeignBreakoutTag` (html-injection-points.ts:497) — `font`
/// breaks out only with a presentational attribute.
fn is_foreign_breakout_tag(tag_name: &str, scanned: &ScannedTag) -> bool {
    if tag_name == "font" {
        return scanned
            .attrs
            .iter()
            .any(|attr| matches!(attr.as_str(), "color" | "face" | "size"));
    }
    FOREIGN_BREAKOUT_TAGS.contains(&tag_name)
}

/// Lowercase only A–Z, so byte offsets stay aligned (parity: `asciiLower`).
fn ascii_lower_shadow(html: &str) -> String {
    html.chars()
        .map(|c| {
            if c.is_ascii_uppercase() {
                c.to_ascii_lowercase()
            } else {
                c
            }
        })
        .collect()
}

fn ascii_lower_str(value: &str) -> String {
    ascii_lower_shadow(value)
}

fn starts_with(bytes: &[u8], at: usize, prefix: &[u8]) -> bool {
    bytes.len() >= at + prefix.len() && &bytes[at..at + prefix.len()] == prefix
}

fn find_byte(bytes: &[u8], needle: u8, from: usize) -> Option<usize> {
    (from..bytes.len()).find(|&index| bytes[index] == needle)
}

fn find_bytes(bytes: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(from);
    }
    (from..=bytes.len().saturating_sub(needle.len()))
        .find(|&index| starts_with(bytes, index, needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_basic_src_and_href() {
        let html = r#"<img src="./assets/logo.png"><a href='assets/x.svg'></a>"#;
        let out = rewrite_skill_asset_urls(html, "demo", "");
        assert_eq!(
            out,
            r#"<img src="/api/skills/demo/assets/logo.png"><a href='/api/skills/demo/assets/x.svg'></a>"#
        );
    }

    #[test]
    fn rewrite_sibling_prefix_repoints_to_that_skill() {
        let html = r#"<img src="../other/assets/a.png">"#;
        let out = rewrite_skill_asset_urls(html, "demo", "");
        assert_eq!(
            out,
            r#"<img src="/api/skills/other/assets/a.png">"#
        );
    }

    #[test]
    fn rewrite_is_case_insensitive_and_needs_whitespace_before_the_attribute() {
        let html = "<IMG SRC=\"./assets/A.PNG\" HREF = 'Assets/B.png' data-src=\"./assets/no.png\">";
        let out = rewrite_skill_asset_urls(html, "demo", "");
        assert_eq!(
            out,
            "<IMG SRC=\"/api/skills/demo/assets/A.PNG\" HREF = '/api/skills/demo/assets/B.png' data-src=\"./assets/no.png\">"
        );
    }

    #[test]
    fn rewrite_appends_the_workspace_query_verbatim() {
        let html = r#"<img src="./assets/logo.png">"#;
        let out = rewrite_skill_asset_urls(html, "demo", "?workspaceId=w&workspaceMemberId=m");
        assert_eq!(
            out,
            r#"<img src="/api/skills/demo/assets/logo.png?workspaceId=w&workspaceMemberId=m">"#
        );
    }

    #[test]
    fn rewrite_skips_urls_with_hash_or_query_fragments() {
        let html = r#"<img src="./assets/a.png?v=2"><img src="./assets/b.png#x">"#;
        let out = rewrite_skill_asset_urls(html, "demo", "");
        assert_eq!(out, html);
    }

    #[test]
    fn rewrite_reencodes_the_skill_id_like_encodeuri() {
        let html = r#"<img src="assets/a.png">"#;
        let out = rewrite_skill_asset_urls(html, "a b/c'd", "");
        assert_eq!(
            out,
            r#"<img src="/api/skills/a%20b%2Fc'd/assets/a.png">"#
        );
    }

    #[test]
    fn assemble_replaces_the_marker_and_retitles_only_the_real_title() {
        let template = "<html><head><title>Seed</title></head><body>\
                        <script>var s = '<title>Fake</title>';</script>\
                        <!-- SLIDES_HERE --></body></html>";
        let out = assemble_example(template, "<section>ONE</section>", "demo-skill");
        assert!(out.contains("<title>demo-skill | OpenDesign Example</title>"));
        assert!(out.contains("<title>Fake</title>"));
        assert!(out.contains("<section>ONE</section>"));
        assert!(!out.contains("SLIDES_HERE"));
    }

    #[test]
    fn assemble_ignores_titles_in_comments_and_attributes() {
        let template = "<!-- <title>In comment</title> -->\
                        <div data-x=\"<title>In attribute</title>\">\
                        <title>Real</title><!-- SLIDES_HERE -->";
        let out = assemble_example(template, "", "t");
        assert!(out.contains("<!-- <title>In comment</title> -->"));
        assert!(out.contains(r#"<title>In attribute</title>"#));
        assert!(out.contains("<title>t | OpenDesign Example</title>"));
    }

    #[test]
    fn assemble_ignores_titles_inside_svg_subtrees() {
        let template = "<svg><title>icon</title></svg><title>Real</title><!-- SLIDES_HERE -->";
        let out = assemble_example(template, "", "t");
        assert!(out.contains("<svg><title>icon</title></svg>"));
        assert!(out.contains("<title>t | OpenDesign Example</title>"));
    }

    #[test]
    fn find_title_returns_none_when_unclosed_or_only_fake() {
        assert_eq!(find_real_title_range("<title>open"), None);
        assert_eq!(find_real_title_range("<script>var a = '<title>x</title>';</script>"), None);
    }

    #[test]
    fn find_title_closes_on_the_real_end_tag_not_a_longer_name() {
        // `</title-page>` inside a raw-text `<title>` is character data: it
        // neither closes the element nor hides the real `</title>` behind it,
        // so the range runs to the real close (here, the end of input).
        let html = "<title>a</title-page></title>";
        let (start, end) = find_real_title_range(html).expect("range");
        assert_eq!(&html[start..end], html);
        // With no real close anywhere, the longer name closes nothing.
        assert_eq!(find_real_title_range("<title>a</title-page>"), None);
    }

    #[test]
    fn split_derived_id_rules() {
        let derived = split_derived_skill_id("parent:child").expect("derived");
        assert_eq!(derived.parent_id, "parent");
        assert_eq!(derived.child_key, "child");
        assert!(split_derived_skill_id("plain").is_none());
        assert!(split_derived_skill_id(":child").is_none());
        assert!(split_derived_skill_id("parent:").is_none());
        assert!(split_derived_skill_id("parent:..").is_none());
        assert!(split_derived_skill_id("parent:.hidden").is_none());
        assert!(split_derived_skill_id("parent:a/b").is_none());
        assert!(split_derived_skill_id("parent:a:b").is_none());
    }

    #[test]
    fn frontmatter_name_variants() {
        assert_eq!(
            frontmatter_name("---\nname: my-skill\n---\nbody"),
            Some("my-skill".to_string())
        );
        assert_eq!(
            frontmatter_name("---\r\nname: \"quoted-skill\"\r\n---\r\n"),
            Some("quoted-skill".to_string())
        );
        // Non-string scalars fall back to the folder name.
        assert_eq!(frontmatter_name("---\nname: 123\n---\n"), None);
        assert_eq!(frontmatter_name("---\nname: true\n---\n"), None);
        assert_eq!(frontmatter_name("---\nname: null\n---\n"), None);
        assert_eq!(frontmatter_name("---\nname:\n---\n"), None);
        assert_eq!(frontmatter_name("---\nname: \n---\n"), None);
        // No frontmatter / unterminated frontmatter.
        assert_eq!(frontmatter_name("name: x\n"), None);
        assert_eq!(frontmatter_name("---\nname: x\n"), None);
        // Nested `od.name` is not the id; later root keys win.
        assert_eq!(
            frontmatter_name("---\nod:\n  name: nested\nname: root\n---\n"),
            Some("root".to_string())
        );
        assert_eq!(
            frontmatter_name("---\nname: first\nname:\n---\n"),
            None
        );
        // Comment lines are skipped.
        assert_eq!(
            frontmatter_name("---\n# name: commented\nname: real\n---\n"),
            Some("real".to_string())
        );
        // Block scalars fall back (documented deviation).
        assert_eq!(frontmatter_name("---\nname: |\n  block\n---\n"), None);
    }

    #[test]
    fn lexical_resolution_containment() {
        let root = Path::new("/data/skills/demo/assets");
        assert_eq!(lexical_resolve_under(root, "logo.png").unwrap(), "logo.png");
        assert_eq!(lexical_resolve_under(root, "sub/../logo.png").unwrap(), "logo.png");
        assert_eq!(lexical_resolve_under(root, "./logo.png").unwrap(), "logo.png");
        assert_eq!(lexical_resolve_under(root, ".").unwrap(), "");
        assert_eq!(lexical_resolve_under(root, "").unwrap(), "");
        assert!(lexical_resolve_under(root, "../secret.txt").is_err());
        assert!(lexical_resolve_under(root, "../../secret.txt").is_err());
        assert!(lexical_resolve_under(root, "/etc/passwd").is_err());
        assert!(lexical_resolve_under(root, "sub/../../secret.txt").is_err());
        // Staying inside after a pop is fine.
        assert_eq!(
            lexical_resolve_under(root, "sub/../logo.png").unwrap(),
            "logo.png"
        );
    }

    #[test]
    fn safe_example_key_rules() {
        assert!(is_safe_example_key("stock-portfolio-live"));
        assert!(is_safe_example_key("a.b_c-1"));
        assert!(!is_safe_example_key(""));
        assert!(!is_safe_example_key(".hidden"));
        assert!(!is_safe_example_key("a:b"));
        assert!(!is_safe_example_key("a/b"));
        assert!(!is_safe_example_key("a b"));
        assert!(!is_safe_example_key("a\u{e9}b"));
    }
}
