//! HTTP routes — parity port of the TypeScript daemon's Express surface
//! (`apps/daemon/src/server.ts` + `src/routes/*`), core subset.

use std::net::SocketAddr;
use std::path::{Path as StdPath, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{ConnectInfo, Path, RawQuery, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio_stream::wrappers::ReceiverStream;
use tower_http::services::ServeDir;

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
    /// Port the listener actually bound, stored once the listener is up
    /// (parity: `resolvedPortRef` in `apps/daemon/src/server.ts`). Seeded from
    /// `config.port` and overwritten with `addr.port()` in
    /// `RunningDaemon::start`, so `port = 0` (ephemeral) resolves correctly for
    /// the same-origin guard in `crate::mcp`.
    pub resolved_port: Arc<AtomicU16>,
    /// In-memory run registry shared by every `/api/runs*` + `/api/chat`
    /// handler (parity: `design.runs` in `apps/daemon/src/server.ts`).
    pub runs: crate::runs::Runs,
    /// Serializes writes to `<data-dir>/mcp-config.json` (parity: the
    /// per-dataDir `writeLocks` promise chain in
    /// `apps/daemon/src/mcp-config.ts`). One `AppState` = one daemon = one
    /// data dir, so a single lock here matches TypeScript's per-path lock.
    pub mcp_write_lock: Arc<tokio::sync::Mutex<()>>,
    /// 5s TTL cache for `GET /api/mcp/install-info` (parity: the
    /// `installInfoCache` closure variable in `registerMcpRoutes`,
    /// `apps/daemon/src/mcp-routes.ts:33`). A std mutex: `crate::mcp` only
    /// holds it across synchronous work, never an `.await`.
    pub mcp_install_info_cache: crate::mcp::InstallInfoCacheSlot,
    /// Serializes writes to `<data-dir>/mcp-tokens.json` (parity: the
    /// per-dataDir `writeLocks` promise chain in `mcp-tokens.ts:163`). One
    /// `AppState` = one daemon = one data dir, exactly like
    /// [`Self::mcp_write_lock`]; the OAuth disconnect route holds it across
    /// read → modify → write.
    pub mcp_tokens_lock: Arc<tokio::sync::Mutex<()>>,
    /// In-memory pending-state cache for the MCP OAuth dance (parity:
    /// `PendingAuthCache` in `mcp-oauth.ts`). State is single-use and
    /// TTL-scoped to the daemon process.
    pub mcp_pending_auth: Arc<tokio::sync::Mutex<std::collections::HashMap<String, crate::mcp_oauth::PendingAuthState>>>,
    /// Live diagnostics snapshots the export bundle drains (environment
    /// evidence, the renderer's chat-scroll capture, recent API failures).
    pub diagnostics: crate::diagnostics::SharedDiagnostics,
    /// The desktop import-token gate: the secret the desktop shell minted at
    /// startup, plus the consumed-nonce set that makes every token
    /// single-use (parity: `apps/daemon/src/desktop-auth.ts`).
    pub desktop_import: Arc<crate::desktop_import::DesktopImportGate>,
}

impl AppState {
    pub fn new(config: DaemonConfig, store: Store) -> Self {
        let resolved_port = Arc::new(AtomicU16::new(config.port));
        let desktop_import = Arc::new(crate::desktop_import::DesktopImportGate::new(
            config.desktop_auth_secret.clone(),
        ));
        Self {
            config: Arc::new(config),
            store,
            started_at: Instant::now(),
            shutting_down: Arc::new(AtomicBool::new(false)),
            resolved_port,
            runs: crate::runs::Runs::default(),
            mcp_write_lock: Arc::new(tokio::sync::Mutex::new(())),
            mcp_install_info_cache: Arc::new(std::sync::Mutex::new(None)),
            mcp_tokens_lock: Arc::new(tokio::sync::Mutex::new(())),
            mcp_pending_auth: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            diagnostics: Arc::new(crate::diagnostics::DiagnosticsState::new()),
            desktop_import,
        }
    }
}

/// Build the full router: API routes under `/api` with token auth applied,
/// plus a static SPA fallback when a web build directory is configured.
///
/// Auth parity: the TypeScript middleware is mounted on `/api` only — static
/// preview/SPA assets are not token-guarded (loopback binding protects them).
/// Opt-in request log for UI acceptance runs (`OD_HTTP_LOG=1`). Off by
/// default: a headless UI pass needs to see which route a click actually
/// reached, but the daemon is not a server whose access log anyone reads.
async fn log_request(request: axum::extract::Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let response = next.run(request).await;
    eprintln!("[http] {method} {path} -> {}", response.status());
    response
}

pub fn build_router(state: AppState) -> Router {
    let api = Router::new()
        .route("/api/health", get(health))
        .route("/api/ready", get(ready))
        .route("/api/version", get(version))
        .route("/api/agents", get(list_agents))
        .route("/api/app-config", get(get_app_config).put(put_app_config))
        .route("/api/dir-exists", post(dir_exists))
        .route("/api/recent-dirs", get(list_recent_dirs))
        .route("/api/workspace/context", get(workspace_context))
        .route("/api/workspace/directory", get(workspace_directory))
        .route("/api/projects", get(list_projects).post(create_project))
        .route("/api/import/folder", post(import_folder))
        .route("/api/projects/{id}", get(get_project).patch(patch_project))
        .route(
            "/api/projects/{id}/working-dir",
            post(replace_project_working_dir),
        )
        .route(
            "/api/projects/{id}/workspace-scope",
            get(get_project_workspace_scope),
        )
        .route("/api/projects/{id}/files", get(list_project_files))
        .route("/api/projects/{id}/tabs", get(list_project_tabs))
        .route("/api/projects/{id}/events", get(project_events))
        .route("/api/projects/{id}/media/tasks", get(list_media_tasks))
        .route("/api/projects/{id}/files/{*path}", get(serve_project_file))
        .route("/api/projects/{id}/raw/{*path}", get(serve_project_raw))
        .route(
            "/api/projects/{id}/text-preview/{*path}",
            get(project_file_text_preview),
        )
        // Unknown /api paths → JSON 404 instead of the SPA shell. Registered
        // after every concrete route, including the conversation, static
        // resource, and chat-artifact routes.
        .merge(crate::conversations::router())
        .merge(crate::static_resources::router())
        .merge(crate::chat_artifacts::router())
        .merge(crate::runs::router())
        .merge(crate::chat::router())
        .merge(crate::critique::router())
        .merge(crate::proxy::router())
        .merge(crate::mcp::router())
        .merge(crate::diagnostics::router())
        .route(
            "/api/{*rest}",
            get(api_not_found)
                .post(api_not_found)
                .put(api_not_found)
                .patch(api_not_found)
                .delete(api_not_found),
        )
        .layer(middleware::from_fn_with_state(state.clone(), api_auth))
        // Outermost, so it sees the auth middleware's 401s too: every failed
        // `/api` response feeds the diagnostics failure journal.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::diagnostics::api_failure_recorder,
        ));

    let web_dist = state.config.web_dist.clone();

    // Static trees the web app links to. Mounted outside `/api` so they stay
    // unauthenticated, exactly like `express.static` (server.ts:9272 for
    // `/artifacts`, server.ts:9312 for `/frames`).
    let mut router = Router::new()
        .merge(api)
        .nest_service("/artifacts", ServeDir::new(state.config.paths.artifacts_dir()));
    if let Some(frames) = frames_dir() {
        router = router.nest_service("/frames", ServeDir::new(frames));
    }

    let router = match &web_dist {
        Some(dist) => {
            // `ServeDir` serves the export's real files; its fallback decides
            // what an unmatched path gets. The policy lives in
            // `crate::static_spa` so a missing `/_next` asset stays a 404
            // instead of becoming a 200 HTML shell. Non-GET/HEAD requests are
            // routed to the same fallback so Express's `next()` → 404 shape is
            // preserved instead of serving tower-http's 405.
            let spa = Router::new()
                .fallback(spa_fallback)
                .with_state(SpaState { index: dist.join("index.html") });
            router.fallback_service(
                ServeDir::new(dist)
                    .call_fallback_on_method_not_allowed(true)
                    .fallback(spa),
            )
        }
        None => router.fallback(not_found),
    };
    let router = if std::env::var_os("OD_HTTP_LOG").is_some() {
        router.layer(middleware::from_fn(log_request))
    } else {
        router
    };
    router.with_state(state)
}

/// State for the SPA shell fallback: the `index.html` to answer with.
#[derive(Clone)]
struct SpaState {
    index: PathBuf,
}

/// `ServeDir`'s fallback: answer the shell for a deep link, 404 for everything
/// else (parity: `registerStaticSpaFallback` in `apps/daemon/src/static-spa.ts`).
async fn spa_fallback(State(spa): State<SpaState>, request: axum::extract::Request) -> Response {
    let accept = request
        .headers()
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok());
    let is_shell = crate::static_spa::is_static_spa_fallback_request(
        &crate::static_spa::SpaFallbackRequest {
            method: request.method(),
            path: request.uri().path(),
            accept,
        },
    );
    if !is_shell {
        return not_found().await;
    }
    match tokio::fs::read(&spa.index).await {
        Ok(bytes) => (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            bytes,
        )
            .into_response(),
        // A missing shell is a plain 404, exactly like
        // `resolveStaticSpaFallbackPath` returning null.
        Err(_) => not_found().await,
    }
}

/// Parity: `FRAMES_DIR` (server.ts:1339) — `OD_RESOURCE_ROOT/frames` when a
/// resource root is configured, else the checkout's `assets/frames`, which is
/// the Rust counterpart of the TypeScript `PROJECT_ROOT/assets/frames`
/// fallback (there is no project root here).
///
/// Mounting nothing when the directory cannot be resolved is equivalent to
/// Express serving a directory that does not exist: `/frames/*` stays a 404
/// either way, because the SPA fallback skips that prefix.
fn frames_dir() -> Option<PathBuf> {
    if let Some(root) = crate::static_resources::resource_root() {
        return Some(root.join("frames"));
    }
    let checkout = StdPath::new(env!("CARGO_MANIFEST_DIR")).join("../../../assets/frames");
    checkout.is_dir().then_some(checkout)
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
        Ok(Ok(projects)) => {
            let projects: Vec<Value> = projects
                .iter()
                .map(|project| {
                    let mut value = project.to_api_json();
                    // The no-scope catalog only ever lists unbound projects.
                    value["workspaceId"] = Value::Null;
                    value
                })
                .collect();
            Json(json!({ "projects": projects })).into_response()
        }
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
    let mut project_json = project.to_api_json();
    project_json["workspaceId"] = match workspace_id {
        Some(id) if !id.trim().is_empty() => Value::String(id),
        _ => Value::Null,
    };
    Json(json!({ "project": project_json, "resolvedDir": resolved_dir })).into_response()
}

/// `PATCH /api/projects/:id` — patch a project (parity:
/// `apps/daemon/src/routes/project/index.ts`).
///
/// The web app renames a project and edits its custom instructions through
/// this route, and the daemon-owned metadata rules live in
/// [`crate::project_patch`]: a patch may repeat the daemon's own bindings but
/// never change them, and an imported project keeps the folder it was
/// imported from.
async fn patch_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let existing = match state.store.get_project(&id) {
        Ok(Some(project)) => project,
        Ok(None) => return api_error(StatusCode::NOT_FOUND, "PROJECT_NOT_FOUND", "not found"),
        Err(err) => return store_error_response(&err),
    };
    let data_dir = state.config.paths.data_dir().to_path_buf();
    let store = state.store.clone();
    let now = crate::conversations::now_ms();
    let result = tokio::task::spawn_blocking(move || {
        let catalogue = project_catalogue(&data_dir);
        let update =
            match crate::project_patch::compose_project_patch(&existing, &body, &catalogue, now) {
                Ok(update) => update,
                Err(err) => {
                    return Err((StatusCode::BAD_REQUEST, err.code, err.message));
                }
            };
        match store.update_project(&id, &update) {
            Ok(Some(project)) => Ok(project),
            Ok(None) => Err((
                StatusCode::NOT_FOUND,
                "PROJECT_NOT_FOUND".to_string(),
                "not found".to_string(),
            )),
            Err(err) => {
                tracing::error!(error = %err, "storage error");
                Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR".to_string(),
                    "storage error".to_string(),
                ))
            }
        }
    })
    .await;
    match result {
        Ok(Ok(project)) => Json(json!({ "project": project.to_api_json() })).into_response(),
        Ok(Err((status, code, message))) => api_error(status, &code, &message),
        Err(err) => internal_error(&err.to_string()),
    }
}

/// The catalogue ids a project patch may reference (parity: the
/// `listAllSkills` / `listAllDesignSystems` reads behind the TypeScript
/// validators).
fn project_catalogue(data_dir: &StdPath) -> crate::project_patch::Catalogue {
    let skills = crate::skill_catalog::list_skills(&crate::static_resources::skill_roots(data_dir));
    let (user_root, builtin_root) = crate::design_system_catalog::design_system_roots(data_dir);
    let design_systems =
        crate::design_system_catalog::list_all_design_systems(&user_root, builtin_root.as_deref());
    crate::project_patch::Catalogue {
        skills: skills.into_iter().map(|skill| skill.id).collect(),
        design_systems: design_systems
            .into_iter()
            .map(|system| (system.id, system.status))
            .collect(),
    }
}

/// `POST /api/dir-exists` — the composer's live "is this folder still there"
/// probe (parity: `apps/daemon/src/routes/media.ts`).
///
/// The web app treats an unreachable probe as "still there" so a daemon hiccup
/// cannot red-flag a working directory, but a 200 with `exists: false` is what
/// turns the chip red the moment the folder is deleted.
async fn dir_exists(Json(body): Json<Value>) -> Response {
    let path = body
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let exists = if path.is_empty() {
        false
    } else {
        tokio::task::spawn_blocking(move || std::fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false))
            .await
            .unwrap_or(false)
    };
    Json(json!({ "exists": exists })).into_response()
}

/// `GET /api/recent-dirs` — the composer's recent working folders, pruned to
/// those that still exist (parity: the route in `apps/daemon/src/routes/media.ts`).
///
/// The pruned list is written back so the picker never offers a path that no
/// longer resolves. This is also the first read/write of `recentLinkedDirs`
/// through `app_config`, which is why the key is in its allowlist.
async fn list_recent_dirs(State(state): State<AppState>) -> Response {
    let data_dir = state.config.paths.data_dir().to_path_buf();
    let result = tokio::task::spawn_blocking(move || {
        let config = crate::app_config::read_app_config(&data_dir);
        let recents: Vec<String> = config
            .get("recentLinkedDirs")
            .and_then(Value::as_array)
            .map(|dirs| {
                dirs.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let existing: Vec<String> = recents
            .iter()
            .filter(|dir| std::fs::metadata(dir).map(|meta| meta.is_dir()).unwrap_or(false))
            .cloned()
            .collect();
        if existing.len() != recents.len() {
            crate::app_config::write_app_config(
                &data_dir,
                &json!({ "recentLinkedDirs": existing }),
            )
            .map_err(|err| err.to_string())?;
        }
        Ok::<_, String>(existing)
    })
    .await;
    match result {
        Ok(Ok(dirs)) => Json(json!({ "dirs": dirs })).into_response(),
        Ok(Err(message)) => internal_error(&message),
        Err(err) => internal_error(&err.to_string()),
    }
}

/// `GET /api/projects/:id/tabs` — the saved Design Files tab document
/// (parity: `listTabs` behind the route in `apps/daemon/src/routes/project/index.ts`).
async fn list_project_tabs(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if !storage::is_safe_id(&id) {
        return api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", "invalid project id");
    }
    let store = state.store.clone();
    let result = tokio::task::spawn_blocking(move || {
        let project = store.get_project(&id)?;
        match project {
            Some(_) => store.list_tabs(&id).map(Some),
            None => Ok(None),
        }
    })
    .await;
    match result {
        Ok(Ok(Some(tabs))) => Json(tabs).into_response(),
        Ok(Ok(None)) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "project not found" })),
        )
            .into_response(),
        Ok(Err(err)) => store_error_response(&err),
        Err(err) => internal_error(&err.to_string()),
    }
}

/// `GET /api/projects/:id/events` — the project event feed the web app keeps
/// open for the lifetime of a project view.
///
/// The stream opens with the `ready` handshake the client uses to reset its
/// reconnect backoff, then sends keepalive `ping` frames. DOCUMENTED GAP: file
/// watching (`file-changed`) and the in-process project event sinks are not
/// ported, so a change made outside the UI arrives on the client's next
/// snapshot read instead of as a pushed event.
async fn project_events(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if !storage::is_safe_id(&id) {
        return api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", "invalid project id");
    }
    let store = state.store.clone();
    let project_id = id.clone();
    let found = tokio::task::spawn_blocking(move || store.get_project(&project_id))
        .await
        .ok()
        .and_then(Result::ok)
        .flatten()
        .is_some();
    if !found {
        return api_error(StatusCode::NOT_FOUND, "PROJECT_NOT_FOUND", "not found");
    }

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(8);
    tokio::spawn(async move {
        let ready = Event::default()
            .event("ready")
            .data(json!({ "projectId": id }).to_string());
        if tx.send(Ok(ready)).await.is_err() {
            return;
        }
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(25));
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let ping = Event::default().event("ping").data("{}");
            if tx.send(Ok(ping)).await.is_err() {
                break;
            }
        }
    });
    Sse::new(ReceiverStream::new(rx)).into_response()
}

/// `GET /api/workspace/context` — this daemon has no Workspace identity, so the
/// honest answer is a null context (parity: the route in
/// `apps/daemon/src/routes/collab-context.ts`, reduced to its headerless case).
async fn workspace_context() -> Response {
    Json(json!({ "context": null })).into_response()
}

/// `GET /api/workspace/directory` — the signed-in account's Workspace list.
/// This daemon has no AMR account, so it is always empty; the web app reads
/// that as the local, unbound scope.
async fn workspace_directory() -> Response {
    Json(json!({ "items": [], "activeWorkspaceId": null })).into_response()
}

/// `GET /api/app-config` — the persisted app preferences (parity: the route in
/// `apps/daemon/src/routes/media.ts`). The web app reads this before it can
/// leave onboarding.
async fn get_app_config(State(state): State<AppState>) -> Response {
    let data_dir = state.config.paths.data_dir().to_path_buf();
    match tokio::task::spawn_blocking(move || crate::app_config::read_app_config(&data_dir)).await {
        Ok(config) => Json(json!({ "config": config })).into_response(),
        Err(err) => internal_error(&err.to_string()),
    }
}

/// `PUT /api/app-config` — merge and persist preferences.
async fn put_app_config(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let data_dir = state.config.paths.data_dir().to_path_buf();
    match tokio::task::spawn_blocking(move || {
        crate::app_config::write_app_config(&data_dir, &body)
    })
    .await
    {
        Ok(Ok(config)) => Json(json!({ "config": config })).into_response(),
        Ok(Err(message)) => api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", &message),
        Err(err) => internal_error(&err.to_string()),
    }
}

/// `GET /api/projects/{id}/workspace-scope` — parity port of the read in
/// `apps/daemon/src/routes/project/index.ts`.
///
/// The web app derives its project resource authority from this answer, and
/// that authority gates every artifact preview: an unbound scope reads as
/// `local`. The Rust daemon has no Workspace binding table (its rows always
/// serialize `workspaceId: null`), so every project it serves is unbound; a
/// Team-bound project has no representation here yet and must not be invented.
async fn get_project_workspace_scope(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let project = match state.store.get_project(&id) {
        Ok(Some(project)) => project,
        Ok(None) => {
            return api_error(StatusCode::NOT_FOUND, "PROJECT_NOT_FOUND", "not found")
        }
        Err(err) => return store_error_response(&err),
    };
    Json(json!({
        "scope": {
            "kind": "unbound",
            "projectId": project.id,
            "workspaceId": null,
            "context": null,
        }
    }))
    .into_response()
}

/// `GET /api/agents` — the detected agent catalogue (parity: the route in
/// `apps/daemon/src/routes/static-resource.ts`). `?stream=1` answers the same
/// rows as `agent` SSE events followed by `done`, which is what `apps/web`
/// consumes on cold start.
async fn list_agents(RawQuery(query): RawQuery) -> Response {
    let wants_stream = matches!(
        single_query_value(query.as_deref(), "stream").as_deref(),
        Some("1") | Some("true")
    );
    let agents: Vec<Value> = crate::agents::shipped_agent_defs()
        .iter()
        .map(|def| {
            let bin_path = crate::spawn::resolve_bin(def.bin, def.fallback_bins);
            crate::agents::agent_info_json(def, bin_path.as_deref())
        })
        .collect();
    if !wants_stream {
        return Json(json!({ "agents": agents })).into_response();
    }
    let mut body = String::new();
    for agent in &agents {
        body.push_str("event: agent\ndata: ");
        body.push_str(&agent.to_string());
        body.push_str("\n\n");
    }
    body.push_str("event: done\ndata: {}\n\n");
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache, no-transform"),
        ],
        body,
    )
        .into_response()
}

/// `POST /api/projects` — create a managed project (parity:
/// `apps/daemon/src/routes/project/index.ts`). Workspace attribution, catalog
/// validation, and plugin snapshot resolution are not ported (see
/// [`crate::project_create`]).
async fn create_project(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let Some(id) = body.get("id").and_then(Value::as_str) else {
        return api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", "invalid project id");
    };
    if !storage::is_safe_id(id) {
        return api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", "invalid project id");
    }
    let Some(name) = body
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
    else {
        return api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", "name required");
    };
    let mut metadata = match body.get("metadata") {
        None | Some(Value::Null) => None,
        Some(Value::Object(object)) => Some(object.clone()),
        Some(_) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
                "metadata must be an object",
            )
        }
    };
    if let Some(metadata) = &mut metadata {
        if metadata.contains_key("baseDir") {
            return api_error(
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
                "baseDir can only be set via POST /api/import/folder",
            );
        }
        if metadata.contains_key("fromTrustedPicker") {
            return api_error(
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
                "fromTrustedPicker can only be set via POST /api/import/folder",
            );
        }
        if metadata.contains_key("orchestratorWorkspace") {
            return api_error(
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
                "orchestratorWorkspace can only be set via POST /api/import/folder or POST /api/projects/:id/working-dir",
            );
        }
        if let Some(linked_dirs) = metadata.get("linkedDirs") {
            match crate::project_create::validate_linked_dirs(linked_dirs) {
                // Persist the canonical, de-duplicated list, exactly like the
                // TypeScript create path.
                Ok(validated) => {
                    metadata.insert("linkedDirs".to_string(), json!(validated));
                }
                Err(message) => {
                    return api_error(StatusCode::BAD_REQUEST, "INVALID_LINKED_DIR", &message);
                }
            }
        }
    }
    let custom_instructions = match body.get("customInstructions") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) if value.chars().count() <= 5000 => Some(value.clone()),
        Some(Value::String(_)) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
                "customInstructions exceeds 5 000 character limit",
            )
        }
        Some(_) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
                "customInstructions must be a string or null",
            )
        }
    };
    let skip_discovery_brief = match body.get("skipDiscoveryBrief") {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
                "skipDiscoveryBrief must be a boolean",
            )
        }
    };
    if skip_discovery_brief {
        metadata
            .get_or_insert_with(serde_json::Map::new)
            .insert("skipDiscoveryBrief".to_string(), Value::Bool(true));
    }
    let session_mode = body
        .get("conversationMode")
        .or_else(|| body.get("sessionMode"))
        .and_then(Value::as_str)
        .filter(|mode| matches!(*mode, "design" | "ask"))
        .unwrap_or("design")
        .to_string();
    let skill_id = body.get("skillId").and_then(Value::as_str);
    let design_system_id = body.get("designSystemId").and_then(Value::as_str);
    let pending_prompt = body.get("pendingPrompt").and_then(Value::as_str);
    let metadata_json = metadata.map(|metadata| Value::Object(metadata).to_string());

    let store = state.store.clone();
    let id = id.to_string();
    let name = name.to_string();
    let skill_id = skill_id.map(str::to_string);
    let design_system_id = design_system_id.map(str::to_string);
    let pending_prompt = pending_prompt.map(str::to_string);
    let conversation_id = crate::conversations::random_id();
    let conversation_id_for_task = conversation_id.clone();
    let project_dir = state.config.paths.projects_dir().join(&id);
    let result = tokio::task::spawn_blocking(move || {
        let now = crate::conversations::now_ms();
        // The managed project directory is created with the row (parity:
        // `ensureProjectDir` in the TypeScript create path). A run whose cwd
        // does not exist fails at spawn with ENOENT.
        std::fs::create_dir_all(&project_dir)?;
        let project = store.insert_project(storage::NewProject {
            id: &id,
            name: &name,
            skill_id: skill_id.as_deref(),
            design_system_id: design_system_id.as_deref(),
            pending_prompt: pending_prompt.as_deref(),
            metadata_json: metadata_json.as_deref(),
            custom_instructions: custom_instructions.as_deref(),
            created_at: now,
            updated_at: now,
        })?;
        crate::conversations::insert_conversation(
            &store,
            &conversation_id_for_task,
            &id,
            Value::Null,
            &session_mode,
            now,
        )?;
        Ok::<_, StoreError>(project)
    })
    .await;
    match result {
        Ok(Ok(project)) => {
            let mut project_json = project.to_api_json();
            project_json["workspaceId"] = Value::Null;
            Json(json!({ "project": project_json, "conversationId": conversation_id }))
                .into_response()
        }
        Ok(Err(err)) => store_error_response(&err),
        Err(err) => internal_error(&err.to_string()),
    }
}

/// `POST /api/import/folder` — import an existing folder as a project
/// (parity: `apps/daemon/src/import-export-routes.ts`).
///
/// The desktop import token and the `orchestratorWorkspace` contract are
/// ported; workspace attribution (`x-od-workspace-*`) is not — this daemon has
/// no workspace identity, so every import is a headerless local create.
async fn import_folder(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    // The token is verified against the *raw* `baseDir` (the string the
    // desktop minted it over) and the path is resolved from the trimmed one
    // (parity: `import-export-routes.ts`).
    let Some(base_dir) = body.get("baseDir").and_then(Value::as_str) else {
        return api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", "baseDir required");
    };
    if base_dir.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", "baseDir required");
    }
    let orchestrator_workspace =
        match crate::project_create::parse_orchestrator_workspace(body.get("orchestratorWorkspace"))
        {
            Ok(value) => value,
            Err(message) => {
                return api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", &message);
            }
        };
    let trusted_picker = match desktop_import_gate(&state, &headers, base_dir) {
        Ok(trusted) => trusted,
        Err(response) => return *response,
    };
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string);
    let skill_id = body.get("skillId").and_then(Value::as_str).map(str::to_string);
    let design_system_id = body
        .get("designSystemId")
        .and_then(Value::as_str)
        .map(str::to_string);

    let data_dir = state.config.paths.data_dir().to_path_buf();
    let base_dir = base_dir.to_string();
    let project_id = crate::conversations::random_id();
    let conversation_id = crate::conversations::random_id();
    let conversation_id_for_task = conversation_id.clone();
    let store = state.store.clone();
    let result = tokio::task::spawn_blocking(move || {
        // Resolve symlinks once and persist the canonical path (parity: the
        // import route's realpath + lstat sequence).
        let root = resolve_import_root(&base_dir, &data_dir, IMPORT_ROOT_MESSAGES)?;
        let real_path = root.path;
        let entry_file = root.entry_file;
        let project_name = name.unwrap_or_else(|| {
            real_path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "project".to_string())
        });
        let now = crate::conversations::now_ms();
        let mut metadata = serde_json::Map::new();
        metadata.insert("kind".to_string(), Value::String("prototype".to_string()));
        metadata.insert(
            "baseDir".to_string(),
            Value::String(real_path.to_string_lossy().into_owned()),
        );
        metadata.insert(
            "importedFrom".to_string(),
            Value::String("folder".to_string()),
        );
        metadata.insert(
            "entryFile".to_string(),
            entry_file.clone().map(Value::String).unwrap_or(Value::Null),
        );
        if let Some(workspace) = &orchestrator_workspace {
            metadata.insert("orchestratorWorkspace".to_string(), workspace.to_json());
        }
        if trusted_picker {
            metadata.insert("fromTrustedPicker".to_string(), Value::Bool(true));
        }
        let metadata_json = Value::Object(metadata).to_string();
        let project = store.insert_project(storage::NewProject {
            id: &project_id,
            name: &project_name,
            skill_id: skill_id.as_deref(),
            design_system_id: design_system_id.as_deref(),
            pending_prompt: None,
            metadata_json: Some(&metadata_json),
            custom_instructions: None,
            created_at: now,
            updated_at: now,
        })
        .map_err(|err| err.to_string())?;
        crate::conversations::insert_conversation(
            &store,
            &conversation_id_for_task,
            &project_id,
            json!(format!("Imported from {project_name}")),
            "design",
            now,
        )
        .map_err(|err| err.to_string())?;
        // Folder imports land on Design Files so the user picks from the
        // imported artifacts instead of auto-opening the detected file.
        store
            .set_tabs(&project_id, &[], None, now)
            .map_err(|err| err.to_string())?;
        Ok::<_, String>((project, entry_file))
    })
    .await;
    match result {
        Ok(Ok((project, entry_file))) => {
            let mut project_json = project.to_api_json();
            project_json["workspaceId"] = Value::Null;
            Json(json!({
                "project": project_json,
                "conversationId": conversation_id,
                "entryFile": entry_file,
            }))
            .into_response()
        }
        Ok(Err(err)) => api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", &err),
        Err(err) => internal_error(&err.to_string()),
    }
}

/// `POST /api/projects/:id/working-dir` — point an existing project at a
/// different folder (parity: `apps/daemon/src/import-export-routes.ts`).
///
/// The route is the desktop's "replace working directory" flow: the shell
/// picks a folder, mints an import token bound to it, and posts both. The
/// token gate, the folder vetting and the `orchestratorWorkspace` contract are
/// shared with `POST /api/import/folder`; workspace attribution is not
/// ported (this daemon has no workspace identity).
async fn replace_project_working_dir(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !storage::is_safe_id(&id) {
        return api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", "invalid project id");
    }
    let existing = match state.store.get_project(&id) {
        Ok(Some(project)) => project,
        Ok(None) => {
            return api_error(StatusCode::NOT_FOUND, "PROJECT_NOT_FOUND", "project not found");
        }
        Err(err) => return store_error_response(&err),
    };
    // The token is verified against the *raw* `baseDir` (the string the
    // desktop minted it over) and the path is resolved from the trimmed one
    // (parity: `import-export-routes.ts`).
    let Some(base_dir) = body.get("baseDir").and_then(Value::as_str) else {
        return api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", "baseDir required");
    };
    if base_dir.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", "baseDir required");
    }
    let orchestrator_workspace =
        match crate::project_create::parse_orchestrator_workspace(body.get("orchestratorWorkspace"))
        {
            Ok(value) => value,
            Err(message) => {
                return api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", &message);
            }
        };
    let trusted_picker = match desktop_import_gate(&state, &headers, base_dir) {
        Ok(trusted) => trusted,
        Err(response) => return *response,
    };

    let data_dir = state.config.paths.data_dir().to_path_buf();
    let base_dir = base_dir.to_string();
    let store = state.store.clone();
    let existing_metadata = existing
        .metadata_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .and_then(|value| match value {
            Value::Object(object) => Some(object),
            _ => None,
        })
        .unwrap_or_default();
    let result = tokio::task::spawn_blocking(move || {
        let root = resolve_import_root(&base_dir, &data_dir, WORKING_DIR_MESSAGES)?;
        let real_path = root.path;
        let entry_file = root.entry_file;
        // Everything but the orchestrator workspace is preserved: the rebind
        // changes where the project lives, not what it is.
        let mut metadata = existing_metadata;
        metadata.remove("orchestratorWorkspace");
        metadata
            .entry("kind".to_string())
            .or_insert_with(|| Value::String("prototype".to_string()));
        metadata.insert(
            "baseDir".to_string(),
            Value::String(real_path.to_string_lossy().into_owned()),
        );
        metadata.insert(
            "importedFrom".to_string(),
            Value::String("folder".to_string()),
        );
        metadata.insert(
            "entryFile".to_string(),
            entry_file.clone().map(Value::String).unwrap_or(Value::Null),
        );
        if let Some(workspace) = &orchestrator_workspace {
            metadata.insert("orchestratorWorkspace".to_string(), workspace.to_json());
        }
        if trusted_picker {
            metadata.insert("fromTrustedPicker".to_string(), Value::Bool(true));
        }
        let metadata_json = Value::Object(metadata).to_string();
        let now = crate::conversations::now_ms();
        let updated = store
            .update_project_metadata(&id, &metadata_json, now)
            .map_err(|err| err.to_string())?;
        let Some(updated) = updated else {
            return Err("project not found".to_string());
        };
        // Folder imports land on Design Files so the user picks from the
        // imported artifacts instead of auto-opening the detected file.
        store
            .set_tabs(&id, &[], None, now)
            .map_err(|err| err.to_string())?;
        Ok::<_, String>((updated, real_path, entry_file))
    })
    .await;
    match result {
        Ok(Ok((project, real_path, entry_file))) => {
            let mut project_json = project.to_api_json();
            project_json["workspaceId"] = Value::Null;
            Json(json!({
                "project": project_json,
                "baseDir": real_path.to_string_lossy(),
                "entryFile": entry_file,
            }))
            .into_response()
        }
        Ok(Err(err)) if err == "project not found" => {
            api_error(StatusCode::NOT_FOUND, "PROJECT_NOT_FOUND", "project not found")
        }
        Ok(Err(err)) => api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", &err),
        Err(err) => internal_error(&err.to_string()),
    }
}

/// The two 400 messages the two import routes use for the same refusal.
///
/// Parity: `import-export-routes.ts` words the folder-import refusal
/// ("cannot import the …") and the working-dir refusal ("cannot point at the
/// …") differently, and the web app surfaces the message verbatim.
struct ImportRootMessages {
    fs_root: &'static str,
    data_dir: &'static str,
}

/// Parity: the `POST /api/import/folder` wordings.
const IMPORT_ROOT_MESSAGES: ImportRootMessages = ImportRootMessages {
    fs_root: "cannot import the filesystem root",
    data_dir: "cannot import the data directory",
};

/// Parity: the `POST /api/projects/:id/working-dir` wordings.
const WORKING_DIR_MESSAGES: ImportRootMessages = ImportRootMessages {
    fs_root: "cannot point at the filesystem root",
    data_dir: "cannot point at the data directory",
};

/// The folder an import resolved to.
struct ImportRoot {
    /// The canonical (symlink-resolved) path.
    path: PathBuf,
    /// The file a freshly imported folder should open, if any.
    entry_file: Option<String>,
}

/// Resolve and vet the folder a user picked (parity: the shared block of
/// `POST /api/import/folder` and `POST /api/projects/:id/working-dir`).
///
/// The error string is the 400 message the route answers with.
fn resolve_import_root(
    base_dir: &str,
    data_dir: &StdPath,
    messages: ImportRootMessages,
) -> Result<ImportRoot, String> {
    let trimmed = base_dir.trim();
    if !StdPath::new(trimmed).is_absolute() {
        return Err("baseDir must be absolute".to_string());
    }
    // Resolve symlinks once and persist the canonical path. Without this, a
    // user-controlled symlink (e.g. ~/sneaky → /etc) at baseDir would let
    // writeProjectFile escape the project sandbox at every later call:
    // resolveSafe checks the *literal* baseDir, but the OS follows the
    // symlink at write time.
    let real_path = std::fs::canonicalize(trimmed).map_err(|_| "folder not found".to_string())?;
    // The canonical path resolved; re-check it is a real directory rather
    // than another symlink (defence in depth).
    let metadata =
        std::fs::symlink_metadata(&real_path).map_err(|_| "folder not found".to_string())?;
    if !metadata.is_dir() {
        return Err("path must be a directory".to_string());
    }
    if real_path.parent().is_none() {
        return Err(messages.fs_root.to_string());
    }
    // Compare against the canonical data dir: the import folder's realpath
    // and the data dir may spell the same directory differently.
    let canonical_data_dir =
        std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    if real_path == canonical_data_dir || real_path.starts_with(&canonical_data_dir) {
        return Err(messages.data_dir.to_string());
    }
    if crate::project_create::blocked_project_root_reason(&real_path).is_some() {
        return Err("cannot use a system or credential directory as a project root".to_string());
    }
    let entry_file = crate::project_create::detect_entry_file(&real_path);
    Ok(ImportRoot {
        path: real_path,
        entry_file,
    })
}

/// The desktop import-token gate's verdict on one import request (parity: the
/// `isDesktopAuthGateActive()` block shared by the two import routes).
///
/// `Ok(true)` means the request may import *and* the folder came from the
/// desktop picker; `Ok(false)` means the gate is open but the folder was not
/// picked through the desktop. The error case is already an HTTP response.
fn desktop_import_gate(
    state: &AppState,
    headers: &HeaderMap,
    base_dir: &str,
) -> Result<bool, Box<Response>> {
    let token = headers
        .get(crate::desktop_import::DESKTOP_IMPORT_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok());
    match state
        .desktop_import
        .authorize(base_dir, token, crate::desktop_import::now_ms())
    {
        crate::desktop_import::Decision::Open => Ok(false),
        crate::desktop_import::Decision::Trusted => Ok(true),
        crate::desktop_import::Decision::Pending => {
            let mut details = serde_json::Map::new();
            details.insert(
                "hint".to_string(),
                Value::String("restart desktop or wait for sidecar registration".to_string()),
            );
            let mut extra = serde_json::Map::new();
            extra.insert("details".to_string(), Value::Object(details));
            extra.insert("retryable".to_string(), Value::Bool(true));
            Err(Box::new(api_error_with(
                StatusCode::SERVICE_UNAVAILABLE,
                "DESKTOP_AUTH_PENDING",
                "desktop auth required but secret not yet registered",
                extra,
            )))
        }
        crate::desktop_import::Decision::Rejected(reason) => {
            let mut details = serde_json::Map::new();
            details.insert("reason".to_string(), Value::String(reason));
            let mut extra = serde_json::Map::new();
            extra.insert("details".to_string(), Value::Object(details));
            Err(Box::new(api_error_with(
                StatusCode::FORBIDDEN,
                "FORBIDDEN",
                "desktop import token rejected",
                extra,
            )))
        }
    }
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

/// `GET /api/projects/:id/media/tasks` — parity port of the TS media task
/// list (`apps/daemon/src/routes/media.ts`), reading the durable `media_tasks`
/// rows the daemon already stores.
///
/// DOCUMENTED GAP: the TS route also recovers a `done` task whose file was
/// moved by re-resolving it by size/mtime across the project inventory. This
/// port only confirms a file that still exists under its recorded name; when
/// it does not, the task omits `file` (the same shape TS produces when no
/// candidate can be confirmed).
async fn list_media_tasks(
    State(state): State<AppState>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    let project = match load_project_for_files(&state, &id).await {
        Ok(project) => project,
        Err(response) => return *response,
    };
    let include_done = matches!(
        single_query_value(query.as_deref(), "includeDone").as_deref(),
        Some("1") | Some("true")
    );
    let store = state.store.clone();
    let project_id = project.id.clone();
    let rows = match tokio::task::spawn_blocking(move || {
        crate::media_tasks::list_media_tasks_by_project(&store, &project_id, include_done)
    })
    .await
    {
        Ok(Ok(rows)) => rows,
        Ok(Err(err)) => return store_error_response(&err),
        Err(err) => return internal_error(&err.to_string()),
    };

    // Confirm the files a finished task claims. Only `done` rows can have a
    // file, and only when `includeDone` asked for them.
    let mut confirmed: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    if include_done && rows.iter().any(|row| row.status == "done") {
        if let Ok(base) = project_fs_base(&state, &project) {
            let inventory = tokio::task::spawn_blocking(move || {
                project_files::list_files(&base, None)
            })
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
            let by_name: std::collections::HashMap<String, Value> = inventory
                .into_iter()
                .filter_map(|entry| {
                    let name = entry.get("name").and_then(Value::as_str)?.to_string();
                    Some((name, entry))
                })
                .collect();
            for row in &rows {
                if row.status != "done" {
                    continue;
                }
                let name = row
                    .file
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|name| !name.is_empty());
                if let Some(entry) = name.and_then(|name| by_name.get(name)) {
                    confirmed.insert(row.id.clone(), entry.clone());
                }
            }
        }
    }

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0);
    let payload = crate::media_tasks::media_task_list_payload(&rows, &confirmed, now_ms);
    ([(header::CACHE_CONTROL, "no-store")], Json(payload)).into_response()
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

/// `GET /api/projects/:id/raw/*path` — the bytes the artifact preview iframe
/// loads (parity: the `/raw/` route in `apps/daemon/src/routes/project/index.ts`).
///
/// The viewer issues a HEAD first and refuses to render without it, so this
/// route is what turns a persisted artifact into a visible one. It answers with
/// a weak `size-mtime` ETag and `no-cache` so a rewritten artifact is picked up
/// on the next load while unchanged bytes revalidate to 304.
///
/// DOCUMENTED GAPS: the preview bridges (URL rewriting, Vite preview
/// resolution, workspace-scoped asset URLs) are not applied — the daemon serves
/// the stored bytes as-is. A sandboxed preview iframe sends `Origin: null`, so
/// that case gets `Access-Control-Allow-Origin: *`, exactly like the port.
async fn serve_project_raw(
    State(state): State<AppState>,
    Path((id, path)): Path<(String, String)>,
    headers: HeaderMap,
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
        tokio::task::spawn_blocking(move || project_files::read_raw_file(&base, &path, imported))
            .await;
    let file = match result {
        Ok(Ok(file)) => file,
        Ok(Err(err)) => return project_path_error_response(err),
        Err(err) => return internal_error(&err.to_string()),
    };

    let etag = format!("W/\"{:x}-{:x}\"", file.size, file.mtime_ms);
    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&file.mime)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    response_headers.insert(
        header::ETAG,
        HeaderValue::from_str(&etag).unwrap_or_else(|_| HeaderValue::from_static("W/\"0-0\"")),
    );
    response_headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        == Some("null")
    {
        response_headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    }
    let fresh = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(',').any(|tag| tag.trim() == etag));
    if fresh {
        return (StatusCode::NOT_MODIFIED, response_headers).into_response();
    }
    (response_headers, file.bytes).into_response()
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
pub(crate) fn single_query_value(query: Option<&str>, key: &str) -> Option<String> {
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

/// The API error envelope with the extra fields the TypeScript
/// `sendApiError(res, …, init)` accepts (parity:
/// `apps/daemon/src/http/api-errors.ts` — `details`, `retryable`, …).
pub(crate) fn api_error_with(
    status: StatusCode,
    code: &str,
    message: &str,
    extra: serde_json::Map<String, Value>,
) -> Response {
    let mut error = serde_json::Map::new();
    error.insert("code".to_string(), Value::String(code.to_string()));
    error.insert("message".to_string(), Value::String(message.to_string()));
    for (key, value) in extra {
        error.insert(key, value);
    }
    (status, Json(json!({ "error": Value::Object(error) }))).into_response()
}

pub(crate) fn store_error_response(err: &StoreError) -> Response {
    tracing::error!(error = %err, "storage error");
    api_error(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", "storage error")
}

pub(crate) fn internal_error(message: &str) -> Response {
    tracing::error!(error = %message, "internal error");
    api_error(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", "internal error")
}
