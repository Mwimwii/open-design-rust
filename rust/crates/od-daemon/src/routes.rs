//! HTTP routes — parity port of the TypeScript daemon's Express surface
//! (`apps/daemon/src/server.ts` + `src/routes/*`), core subset.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{ConnectInfo, Path, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};
use tower_http::services::{ServeDir, ServeFile};

use crate::auth;
use crate::config::DaemonConfig;
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
        // Unknown /api paths → JSON 404 instead of the SPA shell.
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

    let resolved_dir = match resolved_dir(&state.config, &project) {
        Ok(dir) => dir,
        Err(message) => {
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, "PROJECT_DIR_UNRESOLVED", &message);
        }
    };
    let mut project_json = serde_json::to_value(&project).unwrap_or(Value::Null);
    project_json["workspaceId"] = match workspace_id {
        Some(id) if !id.trim().is_empty() => Value::String(id),
        _ => Value::Null,
    };
    Json(json!({ "project": project_json, "resolvedDir": resolved_dir })).into_response()
}

/// Managed dir = `<data root>/projects/<id>`; imported projects use their
/// external `metadata.baseDir`. Port of `resolveProjectDir` (sandbox-mode
/// allowlist rules are a follow-up, tracked in beads).
fn resolved_dir(config: &DaemonConfig, project: &ProjectRow) -> Result<String, String> {
    if let Some(base_dir) = external_base_dir(project) {
        return Ok(base_dir);
    }
    if !storage::is_safe_id(&project.id) {
        return Err(format!("invalid project id: {}", project.id));
    }
    Ok(config.paths.projects_dir().join(&project.id).to_string_lossy().into_owned())
}

/// `metadata.baseDir` is the external workspace root when present. The
/// TypeScript `usesExternalProjectRoot` additionally consults sandbox
/// allowlists; in sandbox mode we conservatively stay on the managed root.
fn external_base_dir(project: &ProjectRow) -> Option<String> {
    let metadata: Value = serde_json::from_str(project.metadata_json.as_deref()?).ok()?;
    let base_dir = metadata.get("baseDir")?.as_str()?.trim();
    if base_dir.is_empty() {
        return None;
    }
    let sandbox = matches!(
        std::env::var("OD_SANDBOX_MODE").ok().as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    );
    if sandbox {
        // TODO(sandbox): port `isSandboxImportedProjectRootAllowed`.
        return None;
    }
    Some(base_dir.to_string())
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

fn api_error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({ "error": { "code": code, "message": message } })),
    )
        .into_response()
}

fn store_error_response(err: &StoreError) -> Response {
    tracing::error!(error = %err, "storage error");
    api_error(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", "storage error")
}

fn internal_error(message: &str) -> Response {
    tracing::error!(error = %message, "internal error");
    api_error(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", "internal error")
}
