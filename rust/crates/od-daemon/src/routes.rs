//! HTTP routes — parity port of the TypeScript daemon's Express surface
//! (`apps/daemon/src/server.ts` + `src/routes/*`), core subset.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{ConnectInfo, Path, RawQuery, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};
use tower_http::services::{ServeDir, ServeFile};

use crate::auth;
use crate::config::DaemonConfig;
use crate::project_dir::{self, ProjectDirError, ProjectPathError};
use crate::project_files::{
    self, DEFAULT_TEXT_PREVIEW_LIMIT, MAX_TEXT_PREVIEW_LIMIT, MIN_TEXT_PREVIEW_LIMIT,
};
use crate::storage::{self, ProjectRow, Store, StoreError};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<DaemonConfig>,
    pub store: Store,
    pub started_at: Instant,
    pub shutting_down: Arc<AtomicBool>,
}

impl AppState {
    pub fn new(config: DaemonConfig, store: Store) -> Self {
        Self {
            config: Arc::new(config),
            store,
            started_at: Instant::now(),
            shutting_down: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// Build the full router: API routes under `/api` with token auth applied,
/// plus a static SPA fallback when a web build directory is configured.
///
/// Auth parity: the TypeScript middleware is mounted on `/api` only — static
/// preview/SPA assets are not token-guarded (loopback binding protects them).
pub fn build_router(state: AppState) -> Router {
    let api = Router::new()
        .route("/api/health", get(health))
        .route("/api/ready", get(ready))
        .route("/api/version", get(version))
        .route("/api/projects", get(list_projects))
        .route("/api/projects/{id}", get(get_project))
        .route("/api/projects/{id}/files", get(list_project_files))
        .route("/api/projects/{id}/files/{*path}", get(serve_project_file))
        .route(
            "/api/projects/{id}/text-preview/{*path}",
            get(project_file_text_preview),
        )
        // Unknown /api paths → JSON 404 instead of the SPA shell. Registered
        // after every concrete route, including the conversation routes.
        .merge(crate::conversations::router())
        .route(
            "/api/{*rest}",
            get(api_not_found)
                .post(api_not_found)
                .put(api_not_found)
                .patch(api_not_found)
                .delete(api_not_found),
        )
        .layer(middleware::from_fn_with_state(state.clone(), api_auth));

    let web_dist = state.config.web_dist.clone();
    let router = Router::new().merge(api);
    let router = match &web_dist {
        Some(dist) => {
            let index = dist.join("index.html");
            router.fallback_service(ServeDir::new(dist).fallback(ServeFile::new(index)))
        }
        None => router.fallback(not_found),
    };
    router.with_state(state)
}

/// `GET /api/health` — same probe shape as the TypeScript daemon, including
/// the AMR terminal reporter block (zeros: no outbox in the Rust daemon yet).
async fn health(State(_state): State<AppState>) -> impl IntoResponse {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "ok": true,
            "version": env!("CARGO_PKG_VERSION"),
            "amrTerminalReporter": {
                "status": "active",
                "pending": 0,
                "delivered": 0,
                "unsupported": 0,
                "terminalFailed": 0,
                "oldestPendingAgeMs": Value::Null,
            },
        })),
    )
}

/// `GET /api/ready` — 503 while the daemon is shutting down.
async fn ready(State(state): State<AppState>) -> Response {
    let shutting_down = state.shutting_down.load(Ordering::SeqCst);
    let status = if shutting_down {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "ok": !shutting_down,
            "ready": !shutting_down,
            "version": env!("CARGO_PKG_VERSION"),
        })),
    )
        .into_response()
}

/// `GET /api/version` — runtime capability advertisement rides on the version
/// payload (parity: `AppVersionResponse` in `@open-design/contracts`).
async fn version(State(_state): State<AppState>) -> impl IntoResponse {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "version": {
                "version": env!("CARGO_PKG_VERSION"),
                "channel": "dev",
                "packaged": false,
                "platform": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
                "capabilities": { "slideRenderer": false },
            },
        })),
    )
}

/// `GET /api/projects` — the no-scope catalog: unbound projects only.
async fn list_projects(State(state): State<AppState>) -> Response {
    let store = state.store.clone();
    match tokio::task::spawn_blocking(move || store.list_unbound_projects()).await {
        Ok(Ok(projects)) => Json(json!({ "projects": projects })).into_response(),
        Ok(Err(err)) => store_error_response(&err),
        Err(join) => internal_error(&join.to_string()),
    }
}

/// `GET /api/projects/:id` — full project row + workspace binding + resolved
/// on-disk directory (parity: `ProjectResponse`).
async fn get_project(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let store = state.store.clone();
    let id_for_task = id.clone();
    let result = tokio::task::spawn_blocking(move || {
        let project = store.get_project(&id_for_task)?;
        let workspace_id = store.workspace_id_for_project(&id_for_task)?;
        Ok::<_, StoreError>((project, workspace_id))
    })
    .await;
    let (project, workspace_id) = match result {
        Ok(Ok(pair)) => pair,
        Ok(Err(err)) => return store_error_response(&err),
        Err(err) => return internal_error(&err.to_string()),
    };
    let Some(project) = project else {
        return api_error(StatusCode::NOT_FOUND, "PROJECT_NOT_FOUND", "not found");
    };

    let resolved_dir = match project_dir::resolved_dir(&state.config, &project) {
        Ok(dir) => dir,
        Err(err) => return project_dir_error_response(err),
    };
    let mut project_json = serde_json::to_value(&project).unwrap_or(Value::Null);
    project_json["workspaceId"] = match workspace_id {
        Some(id) if !id.trim().is_empty() => Value::String(id),
        _ => Value::Null,
    };
    Json(json!({ "project": project_json, "resolvedDir": resolved_dir })).into_response()
}

/// `GET /api/projects/:id/files` — the project's file inventory (parity:
/// the `files` route in `apps/daemon/src/routes/project/index.ts`).
async fn list_project_files(
    State(state): State<AppState>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    let since = since_filter(query.as_deref());
    let project = match load_project_for_files(&state, &id).await {
        Ok(project) => project,
        Err(response) => return *response,
    };
    let base = match project_fs_base(&state, &project) {
        Ok(base) => base,
        Err(response) => return *response,
    };
    match tokio::task::spawn_blocking(move || project_files::list_files(&base, since)).await {
        // Transport caches must always revalidate this dynamic inventory.
        Ok(Ok(files)) => (
            [(header::CACHE_CONTROL, "no-store")],
            Json(json!({ "files": files })),
        )
            .into_response(),
        Ok(Err(err)) => api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", &err.to_string()),
        Err(err) => internal_error(&err.to_string()),
    }
}

/// `GET /api/projects/:id/files/*path` — raw file bytes + MIME type.
async fn serve_project_file(
    State(state): State<AppState>,
    Path((id, path)): Path<(String, String)>,
) -> Response {
    if project_files::is_project_file_version_path(&path) {
        return api_error(StatusCode::NOT_FOUND, "FILE_NOT_FOUND", "file not found");
    }
    let project = match load_project_for_files(&state, &id).await {
        Ok(project) => project,
        Err(response) => return *response,
    };
    let base = match project_fs_base(&state, &project) {
        Ok(base) => base,
        Err(response) => return *response,
    };
    let imported = project_dir::has_external_project_root(&project);
    let result =
        tokio::task::spawn_blocking(move || project_files::read_project_file(&base, &path, imported))
            .await;
    match result {
        Ok(Ok((bytes, mime))) => {
            let content_type = HeaderValue::from_str(&mime)
                .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
            ([(header::CONTENT_TYPE, content_type)], bytes).into_response()
        }
        Ok(Err(err)) => project_path_error_response(err),
        Err(err) => internal_error(&err.to_string()),
    }
}

/// `GET /api/projects/:id/text-preview/*path?limit=` — bounded UTF-8 preview
/// plus file metadata and the powered-preview capability hint.
async fn project_file_text_preview(
    State(state): State<AppState>,
    Path((id, path)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Response {
    if project_files::is_project_file_version_path(&path) {
        return api_error(StatusCode::NOT_FOUND, "FILE_NOT_FOUND", "file not found");
    }
    let limit = preview_limit(query.as_deref());
    let project = match load_project_for_files(&state, &id).await {
        Ok(project) => project,
        Err(response) => return *response,
    };
    let base = match project_fs_base(&state, &project) {
        Ok(base) => base,
        Err(response) => return *response,
    };
    let imported = project_dir::has_external_project_root(&project);
    let result = tokio::task::spawn_blocking(move || {
        project_files::text_preview(&base, &path, imported, limit)
    })
    .await;
    match result {
        Ok(Ok(preview)) => ([(header::CACHE_CONTROL, "no-store")], Json(preview)).into_response(),
        Ok(Err(err)) => project_path_error_response(err),
        Err(err) => internal_error(&err.to_string()),
    }
}

/// Parity: `getProject` for the file routes. Every id must survive
/// `isSafeId` before it is ever used as a path segment (400), then the row
/// lookup decides 404.
async fn load_project_for_files(state: &AppState, id: &str) -> Result<ProjectRow, Box<Response>> {
    if !storage::is_safe_id(id) {
        return Err(Box::new(api_error(
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
            "invalid project id",
        )));
    }
    let store = state.store.clone();
    let id = id.to_string();
    match tokio::task::spawn_blocking(move || store.get_project(&id)).await {
        Ok(Ok(Some(project))) => Ok(project),
        Ok(Ok(None)) => Err(Box::new(api_error(
            StatusCode::NOT_FOUND,
            "PROJECT_NOT_FOUND",
            "project not found",
        ))),
        Ok(Err(err)) => Err(Box::new(store_error_response(&err))),
        Err(err) => Err(Box::new(internal_error(&err.to_string()))),
    }
}

fn project_fs_base(state: &AppState, project: &ProjectRow) -> Result<PathBuf, Box<Response>> {
    project_dir::project_fs_base(&state.config, project).map_err(|err| {
        Box::new(project_dir_error_response(err))
    })
}

/// Sandbox refusals are client errors (parity: the TypeScript routes catch
/// `SandboxImportedProjectError` and answer 400); an unsafe project id is an
/// unresolved managed root.
fn project_dir_error_response(err: ProjectDirError) -> Response {
    match err {
        ProjectDirError::Rejected(message) => {
            api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", &message)
        }
        ProjectDirError::Unresolved(message) => {
            api_error(StatusCode::INTERNAL_SERVER_ERROR, "PROJECT_DIR_UNRESOLVED", &message)
        }
    }
}

/// Path-refusal split shared by the read routes: missing → 404, everything
/// else → 400. Messages never carry absolute paths.
fn project_path_error_response(err: ProjectPathError) -> Response {
    match err {
        ProjectPathError::NotFound => {
            api_error(StatusCode::NOT_FOUND, "FILE_NOT_FOUND", "file not found")
        }
        other => api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", &other.to_string()),
    }
}

/// Parity: `Number.isFinite(Number(query.since)) && since > 0`.
fn since_filter(query: Option<&str>) -> Option<f64> {
    let value = js_number(&single_query_value(query, "since")?);
    (value.is_finite() && value > 0.0).then_some(value)
}

/// Parity: `Math.max(1024, Math.min(finite ? Math.floor(limit) : 96 * 1024, 512 * 1024))`.
fn preview_limit(query: Option<&str>) -> u64 {
    let requested = match single_query_value(query, "limit") {
        Some(raw) => {
            let value = js_number(&raw);
            if value.is_finite() {
                value.floor()
            } else {
                DEFAULT_TEXT_PREVIEW_LIMIT as f64
            }
        }
        None => DEFAULT_TEXT_PREVIEW_LIMIT as f64,
    };
    requested.clamp(MIN_TEXT_PREVIEW_LIMIT as f64, MAX_TEXT_PREVIEW_LIMIT as f64) as u64
}

/// First (and only) value for `key`, percent-decoded the way Express' query
/// parser decodes it. A repeated key collapses to `None` because coercing an
/// array with `Number(...)` yields `NaN` in JavaScript.
fn single_query_value(query: Option<&str>, key: &str) -> Option<String> {
    let query = query?;
    let mut found: Option<String> = None;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (raw_key, raw_value) = pair.split_once('=').unwrap_or((pair, ""));
        if percent_decode(raw_key) != key {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = Some(percent_decode(raw_value));
    }
    found
}

/// `Number(...)` coercion for query strings: whitespace-trimmed, empty → 0,
/// anything unparsable → `NaN`.
fn js_number(raw: &str) -> f64 {
    let trimmed = raw.trim_matches(char::is_whitespace);
    if trimmed.is_empty() {
        return 0.0;
    }
    match trimmed.parse::<f64>() {
        Ok(value) => {
            let bare = trimmed.trim_start_matches(['+', '-']);
            if value.is_infinite() && !bare.eq_ignore_ascii_case("infinity") {
                f64::NAN
            } else {
                value
            }
        }
        Err(_) => f64::NAN,
    }
}

/// `decodeURIComponent` + `+` → space, matching the Express query parser.
fn percent_decode(raw: &str) -> String {
    if !raw.contains('%') && !raw.contains('+') {
        return raw.to_string();
    }
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                match (hex_digit(bytes[index + 1]), hex_digit(bytes[index + 2])) {
                    (Some(high), Some(low)) => {
                        out.push(high * 16 + low);
                        index += 3;
                    }
                    _ => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| raw.to_string())
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

async fn api_not_found() -> Response {
    api_error(StatusCode::NOT_FOUND, "NOT_FOUND", "Not found")
}

async fn not_found() -> Response {
    api_error(StatusCode::NOT_FOUND, "NOT_FOUND", "Not found")
}

/// Token auth for `/api` routes (parity: `server.ts` middleware), applied as a
/// router layer so static/SPA assets stay unauthenticated like Express.
async fn api_auth(State(state): State<AppState>, request: axum::extract::Request, next: Next) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0);
    let authorization = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let allowed = auth::authorize(
        auth::AuthRequest {
            path: request.uri().path(),
            peer,
            authorization,
        },
        state.config.active_api_token(),
    );
    if allowed {
        return next.run(request).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [
            (header::WWW_AUTHENTICATE, "Basic realm=\"OpenDesign\", charset=\"UTF-8\""),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Json(json!({ "error": { "code": "UNAUTHORIZED", "message": "unauthorized" } })),
    )
        .into_response()
}

pub(crate) fn api_error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({ "error": { "code": code, "message": message } })),
    )
        .into_response()
}

pub(crate) fn store_error_response(err: &StoreError) -> Response {
    tracing::error!(error = %err, "storage error");
    api_error(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", "storage error")
}

pub(crate) fn internal_error(message: &str) -> Response {
    tracing::error!(error = %message, "internal error");
    api_error(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", "internal error")
}
