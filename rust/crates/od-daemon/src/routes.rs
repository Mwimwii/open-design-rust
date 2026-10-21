//! HTTP routes — parity port of the TypeScript daemon's Express surface
//! (`apps/daemon/src/server.ts` + `src/routes/*`), core subset.

use std::net::SocketAddr;
use std::path::{Path as StdPath, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{ConnectInfo, Path, RawQuery, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
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
}

impl AppState {
    pub fn new(config: DaemonConfig, store: Store) -> Self {
        let resolved_port = Arc::new(AtomicU16::new(config.port));
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
        .route("/api/agents", get(list_agents))
        .route("/api/projects", get(list_projects).post(create_project))
        .route("/api/import/folder", post(import_folder))
        .route("/api/projects/{id}", get(get_project))
        .route("/api/projects/{id}/files", get(list_project_files))
        .route("/api/projects/{id}/media/tasks", get(list_media_tasks))
        .route("/api/projects/{id}/files/{*path}", get(serve_project_file))
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
    if let Some(metadata) = &metadata {
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
            if let Err(message) = crate::project_create::validate_linked_dirs(linked_dirs) {
                return api_error(StatusCode::BAD_REQUEST, "INVALID_LINKED_DIR", &message);
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
    let result = tokio::task::spawn_blocking(move || {
        let now = crate::conversations::now_ms();
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
/// (parity: `apps/daemon/src/import-export-routes.ts`). The desktop import
/// token and workspace attribution are not ported; every import is treated as
/// a headerless local create.
async fn import_folder(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let Some(base_dir) = body
        .get("baseDir")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|dir| !dir.is_empty())
    else {
        return api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", "baseDir required");
    };
    if !StdPath::new(base_dir).is_absolute() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
            "baseDir must be absolute",
        );
    }
    if body
        .get("orchestratorWorkspace")
        .is_some_and(|value| !value.is_null())
    {
        return api_error(
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
            "orchestratorWorkspace is not supported by the Rust daemon",
        );
    }
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
        let real_path = std::fs::canonicalize(&base_dir)
            .map_err(|_| "folder not found".to_string())?;
        let metadata =
            std::fs::metadata(&real_path).map_err(|_| "folder not found".to_string())?;
        if !metadata.is_dir() {
            return Err("path must be a directory".to_string());
        }
        if real_path.parent().is_none() {
            return Err("cannot import the filesystem root".to_string());
        }
        let canonical_data_dir = std::fs::canonicalize(&data_dir).unwrap_or(data_dir);
        if real_path == canonical_data_dir || real_path.starts_with(&canonical_data_dir) {
            return Err("cannot import the data directory".to_string());
        }
        if crate::project_create::blocked_project_root_reason(&real_path).is_some() {
            return Err(
                "cannot use a system or credential directory as a project root".to_string(),
            );
        }
        let project_name = name.unwrap_or_else(|| {
            real_path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "project".to_string())
        });
        let entry_file = crate::project_create::detect_entry_file(&real_path);
        let now = crate::conversations::now_ms();
        let metadata_json = json!({
            "kind": "prototype",
            "baseDir": real_path.to_string_lossy(),
            "importedFrom": "folder",
            "entryFile": entry_file,
        })
        .to_string();
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

pub(crate) fn store_error_response(err: &StoreError) -> Response {
    tracing::error!(error = %err, "storage error");
    api_error(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", "storage error")
}

pub(crate) fn internal_error(message: &str) -> Response {
    tracing::error!(error = %message, "internal error");
    api_error(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", "internal error")
}
