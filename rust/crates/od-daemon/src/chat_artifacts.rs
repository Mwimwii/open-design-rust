//! Chat artifact read routes — parity port of
//! `apps/daemon/src/routes/project/chat-artifacts.ts` (its five GET routes)
//! plus the helpers those routes call: the `chat-artifacts/store.ts` row
//! reads, the `chat-artifacts/refs.ts` DTO projection, and the
//! `chat-artifacts/blob-store.ts` key / verification rules.
//!
//! Routes (TypeScript line numbers from `routes/project/chat-artifacts.ts`):
//!
//! * `GET /api/projects/:id/conversations/:cid/messages/:mid/artifacts` (99)
//! * `GET /api/projects/:id/chat-artifact-snapshots/:sid` (123)
//! * `GET /api/projects/:id/chat-artifact-snapshots/:sid/content` (134)
//! * `GET /api/projects/:id/chat-artifact-snapshots/:sid/thumbnail` (142)
//! * `GET /api/projects/:id/workspace-artifacts/:aid` (152)
//!
//! PRIVACY: a caller never supplies a storage key or a path — only an id —
//! and the daemon resolves the key internally (blob-store.ts:82 re-validates
//! the key shape so a corrupted row cannot leave the blob root).
//!
//! DOCUMENTED DEVIATIONS
//!
//! * Workspace authority (`authorizeProjectRequest`) is not ported — the same
//!   headerless lane `conversations.rs` documents: the gate is the project
//!   lookup plus the team-mirror revocation check, in that order.
//! * Blob bytes are read into memory BEFORE the headers are sent, where
//!   TypeScript streams after them: a read failure is always the pre-headers
//!   500 `failed to read snapshot content`, never a truncated body.
//! * `Content-Length` is left to hyper rather than copied from the row; the
//!   verified size and the served size agree by construction.
//! * A storage key whose file exists but is not a regular file fails
//!   verification (TypeScript's `stat` would report its size and the stream
//!   would die after the headers), and an invalid response-header value
//!   falls back to a default instead of becoming a Node `res.set` throw.

use std::path::{Path, PathBuf};

use axum::extract::{Path as PathExtractor, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Map, Value};

use crate::routes::{api_error, internal_error, store_error_response, AppState};
use crate::storage::{Store, StoreError};

/// Parity: the immutable-cache header every verified blob carries.
const IMMUTABLE_CACHE_CONTROL: &str = "private, max-age=31536000, immutable";

/// Parity: `registerProjectChatArtifactRoutes` (chat-artifacts.ts:43). Every
/// route is a GET; the paths use the conversations router's `{id}` name so
/// `matchit` sees one consistent parameter name under `/api/projects/`.
pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/projects/{id}/conversations/{cid}/messages/{mid}/artifacts",
            get(message_artifacts_handler),
        )
        .route(
            "/api/projects/{id}/chat-artifact-snapshots/{sid}",
            get(snapshot_metadata_handler),
        )
        .route(
            "/api/projects/{id}/chat-artifact-snapshots/{sid}/content",
            get(snapshot_content_handler),
        )
        .route(
            "/api/projects/{id}/chat-artifact-snapshots/{sid}/thumbnail",
            get(snapshot_thumbnail_handler),
        )
        .route(
            "/api/projects/{id}/workspace-artifacts/{aid}",
            get(workspace_artifact_handler),
        )
}

// ---- handlers --------------------------------------------------------------

/// `GET …/messages/:mid/artifacts` (chat-artifacts.ts:99).
async fn message_artifacts_handler(
    State(state): State<AppState>,
    PathExtractor((project_id, conversation_id, message_id)): PathExtractor<(
        String,
        String,
        String,
    )>,
) -> Response {
    let store = state.store.clone();
    run_blocking(move || {
        message_artifacts_response(&store, &project_id, &conversation_id, &message_id)
    })
    .await
}

/// `GET …/chat-artifact-snapshots/:sid` (chat-artifacts.ts:123).
async fn snapshot_metadata_handler(
    State(state): State<AppState>,
    PathExtractor((project_id, snapshot_id)): PathExtractor<(String, String)>,
) -> Response {
    let store = state.store.clone();
    run_blocking(move || snapshot_metadata_response(&store, &project_id, &snapshot_id)).await
}

/// `GET …/chat-artifact-snapshots/:sid/content` (chat-artifacts.ts:134).
async fn snapshot_content_handler(
    State(state): State<AppState>,
    PathExtractor((project_id, snapshot_id)): PathExtractor<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let if_none_match = if_none_match_value(&headers);
    let store = state.store.clone();
    let data_dir = state.config.paths.data_dir().to_path_buf();
    run_blocking(move || {
        snapshot_blob_response(
            &store,
            &data_dir,
            &project_id,
            &snapshot_id,
            BlobSide::Content,
            if_none_match.as_deref(),
        )
    })
    .await
}

/// `GET …/chat-artifact-snapshots/:sid/thumbnail` (chat-artifacts.ts:142).
async fn snapshot_thumbnail_handler(
    State(state): State<AppState>,
    PathExtractor((project_id, snapshot_id)): PathExtractor<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let if_none_match = if_none_match_value(&headers);
    let store = state.store.clone();
    let data_dir = state.config.paths.data_dir().to_path_buf();
    run_blocking(move || {
        snapshot_blob_response(
            &store,
            &data_dir,
            &project_id,
            &snapshot_id,
            BlobSide::Thumbnail,
            if_none_match.as_deref(),
        )
    })
    .await
}

/// `GET …/workspace-artifacts/:aid` (chat-artifacts.ts:152).
async fn workspace_artifact_handler(
    State(state): State<AppState>,
    PathExtractor((project_id, artifact_id)): PathExtractor<(String, String)>,
) -> Response {
    let store = state.store.clone();
    run_blocking(move || workspace_artifact_response(&store, &project_id, &artifact_id)).await
}

/// One blocking hop per request (parity: the TypeScript handlers are async
/// and run their `better-sqlite3` calls on the event loop); a panic inside
/// becomes the shared 500 instead of tearing down the connection.
async fn run_blocking<F>(work: F) -> Response
where
    F: FnOnce() -> Response + Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(response) => response,
        Err(err) => internal_error(&err.to_string()),
    }
}

// ---- route bodies ----------------------------------------------------------

/// Parity: the `authorizeRead` gate (chat-artifacts.ts:58) — project lookup,
/// then the team-mirror revocation check. `authorizeProjectRequest` between
/// them is the workspace-authority lane this daemon does not run.
fn authorize_read(store: &Store, project_id: &str) -> Result<(), Box<Response>> {
    let project = match store.get_project(project_id) {
        Ok(project) => project,
        Err(err) => return Err(Box::new(store_error_response(&err))),
    };
    let Some(project) = project else {
        return Err(Box::new(project_not_found()));
    };
    if team_mirror_revoked(project.metadata_json.as_deref()) {
        return Err(Box::new(project_not_found()));
    }
    Ok(())
}

fn project_not_found() -> Response {
    api_error(StatusCode::NOT_FOUND, "PROJECT_NOT_FOUND", "project not found")
}

/// Parity: `project?.metadata?.teamMirrorRevokedAt` truthiness over
/// `JSON.parse(metadata_json)` (db.ts:2063 `normalizeProject` — an
/// unparsable or absent blob is `undefined`, i.e. not revoked).
fn team_mirror_revoked(metadata_json: Option<&str>) -> bool {
    let Some(raw) = metadata_json else {
        return false;
    };
    let Ok(metadata) = serde_json::from_str::<Value>(raw) else {
        return false;
    };
    json_truthy(metadata.get("teamMirrorRevokedAt"))
}

/// JavaScript truthiness over a parsed JSON value (`''`, `0`, `false` and
/// `null` are falsy; absent is `undefined`, also falsy).
fn json_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|value| value != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(_) => true,
    }
}

/// Parity: `snapshotInProject` (chat-artifacts.ts:84) — a snapshot owned by
/// a DIFFERENT project is a 404, not a 403.
fn snapshot_in_project(
    store: &Store,
    project_id: &str,
    snapshot_id: &str,
) -> Result<SnapshotRow, Box<Response>> {
    let snapshot = match get_snapshot(store, snapshot_id) {
        Ok(snapshot) => snapshot,
        Err(err) => return Err(Box::new(store_error_response(&err))),
    };
    match snapshot {
        Some(snapshot) if snapshot.project_id == project_id => Ok(snapshot),
        _ => Err(Box::new(api_error(
            StatusCode::NOT_FOUND,
            "ARTIFACT_NOT_FOUND",
            "snapshot not found",
        ))),
    }
}

fn message_artifacts_response(
    store: &Store,
    project_id: &str,
    conversation_id: &str,
    message_id: &str,
) -> Response {
    if let Err(response) = authorize_read(store, project_id) {
        return *response;
    }
    // Parity: the ownership join (chat-artifacts.ts:104) — the message must
    // sit in the route's conversation AND that conversation in the route's
    // project, or the id never existed as far as this caller is concerned.
    let owned = match store.query_one(
        "SELECT m.id AS id FROM messages m
           JOIN conversations c ON c.id = m.conversation_id
          WHERE m.id = ?1 AND m.conversation_id = ?2 AND c.project_id = ?3",
        rusqlite::params![message_id, conversation_id, project_id],
        |row| row.get::<_, String>("id"),
    ) {
        Ok(owned) => owned,
        Err(err) => return store_error_response(&err),
    };
    if owned.is_none() {
        return api_error(StatusCode::NOT_FOUND, "NOT_FOUND", "message not found");
    }
    match message_refs(store, project_id, message_id) {
        Ok(artifacts) => no_store_json(json!({ "artifacts": artifacts })),
        Err(err) => store_error_response(&err),
    }
}

fn snapshot_metadata_response(store: &Store, project_id: &str, snapshot_id: &str) -> Response {
    if let Err(response) = authorize_read(store, project_id) {
        return *response;
    }
    let snapshot = match snapshot_in_project(store, project_id, snapshot_id) {
        Ok(snapshot) => snapshot,
        Err(response) => return *response,
    };
    match snapshot_metadata(store, &snapshot) {
        Ok(metadata) => no_store_json(json!({ "snapshot": metadata })),
        Err(err) => store_error_response(&err),
    }
}

fn workspace_artifact_response(store: &Store, project_id: &str, artifact_id: &str) -> Response {
    if let Err(response) = authorize_read(store, project_id) {
        return *response;
    }
    let artifact = match get_workspace_artifact(store, artifact_id) {
        Ok(Some(artifact)) if artifact.project_id == project_id => artifact,
        Ok(_) => {
            return api_error(StatusCode::NOT_FOUND, "ARTIFACT_NOT_FOUND", "artifact not found");
        }
        Err(err) => return store_error_response(&err),
    };
    no_store_json(json!({ "artifact": workspace_artifact_metadata(&artifact) }))
}

fn snapshot_blob_response(
    store: &Store,
    data_dir: &Path,
    project_id: &str,
    snapshot_id: &str,
    side: BlobSide,
    if_none_match: Option<&str>,
) -> Response {
    if let Err(response) = authorize_read(store, project_id) {
        return *response;
    }
    let snapshot = match snapshot_in_project(store, project_id, snapshot_id) {
        Ok(snapshot) => snapshot,
        Err(response) => return *response,
    };
    send_snapshot_blob(store, data_dir, &snapshot, side.digest(&snapshot), if_none_match)
}

/// Which half of a snapshot a route serves.
enum BlobSide {
    Content,
    Thumbnail,
}

impl BlobSide {
    fn digest(&self, snapshot: &SnapshotRow) -> Option<String> {
        match self {
            BlobSide::Content => snapshot.content_digest.clone(),
            BlobSide::Thumbnail => snapshot.thumbnail_digest.clone(),
        }
    }
}

/// Parity: `sendSnapshotBlob` (chat-artifacts.ts:176) — honest 404s before
/// any byte moves, size verification before the immutable cache headers, and
/// the exact-string `If-None-Match` check Node performs.
fn send_snapshot_blob(
    store: &Store,
    data_dir: &Path,
    snapshot: &SnapshotRow,
    digest: Option<String>,
    if_none_match: Option<&str>,
) -> Response {
    let digest = digest.filter(|value| !value.is_empty());
    if snapshot.capture_state != "ready" || digest.is_none() {
        // Honest 404 with the reason: the client degrades on this rather than
        // being handed the current workspace file as a stand-in.
        return api_error_details(
            StatusCode::NOT_FOUND,
            "ARTIFACT_NOT_FOUND",
            "snapshot content is not available",
            json!({
                "state": snapshot.capture_state,
                "failureCode": snapshot.failure_code,
            }),
        );
    }
    let digest = digest.expect("checked above");

    let blob = match get_blob(store, &digest) {
        Ok(blob) => blob,
        Err(err) => return store_error_response(&err),
    };
    let Some(blob) = blob else {
        return api_error(
            StatusCode::NOT_FOUND,
            "ARTIFACT_NOT_FOUND",
            "snapshot content is not available",
        );
    };

    // The database's claim is checked against the disk before a single byte
    // is sent. A corrupted store fails loudly instead of serving wrong
    // content under an immutable cache header.
    let Some(path) = verify_blob(data_dir, &blob.storage_key, blob.byte_size) else {
        return api_error_details(
            StatusCode::GONE,
            "ARTIFACT_NOT_FOUND",
            "snapshot content failed verification",
            json!({ "reason": "blob_verification_failed" }),
        );
    };

    let etag = format!("\"{digest}\"");
    // Content addressing makes this genuinely immutable: the bytes behind
    // this id can never change, so a conditional request is always
    // answerable. Parity: exact string equality, not the weak/strong rules.
    if if_none_match == Some(etag.as_str()) {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (header::ETAG, etag.as_str()),
                (header::CACHE_CONTROL, IMMUTABLE_CACHE_CONTROL),
            ],
        )
            .into_response();
    }

    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::error!(error = %err, path = %path.display(), "failed to read snapshot content");
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "failed to read snapshot content",
            );
        }
    };

    let declared = blob
        .mime
        .clone()
        .or_else(|| snapshot.mime.clone())
        .unwrap_or_default();
    let inline = is_inline_safe_mime(&declared);
    let content_type = if inline {
        declared
    } else {
        "application/octet-stream".to_string()
    };

    let mut headers = HeaderMap::new();
    headers.insert(
        header::ETAG,
        HeaderValue::from_str(&etag).unwrap_or_else(|_| HeaderValue::from_static("\"\"")),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(IMMUTABLE_CACHE_CONTROL),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&content_type)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    if !inline {
        // Never let a snapshot render as a document in the app's own origin.
        let disposition = format!("attachment; filename=\"{}\"", safe_filename(&snapshot.source_path_at_capture));
        if let Ok(value) = HeaderValue::from_str(&disposition) {
            headers.insert(header::CONTENT_DISPOSITION, value);
        }
    }
    (StatusCode::OK, headers, bytes).into_response()
}

// ---- request helpers -------------------------------------------------------

/// Node joins duplicate headers with `", "` before the handler sees them.
fn if_none_match_value(headers: &HeaderMap) -> Option<String> {
    let values: Vec<&str> = headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect();
    if values.is_empty() {
        None
    } else {
        Some(values.join(", "))
    }
}

fn no_store_json(body: Value) -> Response {
    ([(header::CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

/// Parity: `sendApiError(res, status, code, message, { details })` — the
/// details ride INSIDE the `error` object (contracts `ApiError.details`).
fn api_error_details(status: StatusCode, code: &str, message: &str, details: Value) -> Response {
    (
        status,
        Json(json!({ "error": { "code": code, "message": message, "details": details } })),
    )
        .into_response()
}

// ---- blob store (chat-artifacts/blob-store.ts) -----------------------------

/// Parity: `BLOB_ROOT_DIRNAME` (blob-store.ts:23) under the daemon data root.
fn blob_root(data_dir: &Path) -> PathBuf {
    data_dir.join("chat-artifact-blobs")
}

/// Parity: `resolveStorageKey` (blob-store.ts:82) — the key shape is
/// re-validated so a corrupted database row cannot traverse out of the blob
/// root. Only well-formed keys resolve; everything else is `None`.
fn resolve_storage_key(data_dir: &Path, storage_key: &str) -> Option<PathBuf> {
    if !is_valid_storage_key(storage_key) {
        return None;
    }
    let root = blob_root(data_dir);
    let resolved = root.join(storage_key);
    if !resolved.starts_with(&root) {
        return None;
    }
    Some(resolved)
}

/// Parity: `OBJECT_KEY_RE` / `TEMP_KEY_RE` (blob-store.ts:27-28) — both are
/// case-sensitive (`[0-9a-f]`, no `i` flag), so uppercase hex is rejected.
fn is_valid_storage_key(storage_key: &str) -> bool {
    if let Some(rest) = storage_key.strip_prefix("objects/") {
        let bytes = rest.as_bytes();
        return bytes.len() == 70
            && is_lower_hex(&bytes[0..2])
            && bytes[2] == b'/'
            && is_lower_hex(&bytes[3..5])
            && bytes[5] == b'/'
            && is_lower_hex(&bytes[6..70]);
    }
    if let Some(rest) = storage_key.strip_prefix("tmp/") {
        let bytes = rest.as_bytes();
        return bytes.len() == 41
            && bytes.ends_with(b".part")
            && bytes[..36]
                .iter()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f' | b'-'));
    }
    false
}

fn is_lower_hex(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Parity: `verifyBlob` (blob-store.ts:211) — `stat` size against the size
/// the database claims. `Some(path)` is the resolved, verified blob.
fn verify_blob(data_dir: &Path, storage_key: &str, expected_byte_size: i64) -> Option<PathBuf> {
    let expected = u64::try_from(expected_byte_size).ok()?;
    let path = resolve_storage_key(data_dir, storage_key)?;
    let metadata = std::fs::metadata(&path).ok()?;
    (metadata.is_file() && metadata.len() == expected).then_some(path)
}

// ---- rows ------------------------------------------------------------------

/// Parity: `ChatArtifactSnapshotRow` (chat-artifacts/store.ts:33), reduced to
/// the columns these read routes project.
struct SnapshotRow {
    id: String,
    project_id: String,
    workspace_artifact_id: Option<String>,
    source_path_at_capture: String,
    kind: String,
    mime: Option<String>,
    content_digest: Option<String>,
    thumbnail_digest: Option<String>,
    run_id: Option<String>,
    media_task_id: Option<String>,
    capture_state: String,
    failure_code: Option<String>,
    created_at: i64,
    ready_at: Option<i64>,
}

const SNAPSHOT_COLS: &str = "id, project_id, workspace_artifact_id, \
     source_path_at_capture, kind, mime, content_digest, thumbnail_digest, \
     run_id, media_task_id, capture_state, failure_code, created_at, ready_at";

fn map_snapshot(row: &rusqlite::Row<'_>) -> rusqlite::Result<SnapshotRow> {
    Ok(SnapshotRow {
        id: row.get("id")?,
        project_id: row.get("project_id")?,
        workspace_artifact_id: row.get("workspace_artifact_id")?,
        source_path_at_capture: row.get("source_path_at_capture")?,
        kind: row.get("kind")?,
        mime: row.get("mime")?,
        content_digest: row.get("content_digest")?,
        thumbnail_digest: row.get("thumbnail_digest")?,
        run_id: row.get("run_id")?,
        media_task_id: row.get("media_task_id")?,
        capture_state: row.get("capture_state")?,
        failure_code: row.get("failure_code")?,
        created_at: row.get("created_at")?,
        ready_at: row.get("ready_at")?,
    })
}

/// Parity: `getChatArtifactSnapshot` (chat-artifacts/store.ts:546).
fn get_snapshot(store: &Store, id: &str) -> Result<Option<SnapshotRow>, StoreError> {
    store.query_one(
        &format!(
            "SELECT {SNAPSHOT_COLS} FROM chat_artifact_snapshots WHERE id = ?1"
        ),
        [id],
        map_snapshot,
    )
}

/// Parity: `ChatArtifactBlobRow` (chat-artifacts/store.ts:69). Only the
/// columns these routes read are selected — `digest` is the lookup key, and
/// the ETag comes from the snapshot's own digest string.
struct BlobRow {
    storage_key: String,
    byte_size: i64,
    mime: Option<String>,
}

/// Parity: `getChatArtifactBlob` (chat-artifacts/store.ts:458).
fn get_blob(store: &Store, digest: &str) -> Result<Option<BlobRow>, StoreError> {
    store.query_one(
        "SELECT storage_key, byte_size, mime FROM chat_artifact_blobs WHERE digest = ?1",
        [digest],
        |row| {
            Ok(BlobRow {
                storage_key: row.get("storage_key")?,
                byte_size: row.get("byte_size")?,
                mime: row.get("mime")?,
            })
        },
    )
}

/// Parity: `WorkspaceArtifactRow` (chat-artifacts/store.ts:19).
struct WorkspaceArtifactRow {
    id: String,
    project_id: String,
    current_path: Option<String>,
    kind: String,
    mime: Option<String>,
    current_digest: Option<String>,
    current_size: Option<i64>,
    current_mtime: Option<i64>,
    created_at: i64,
    updated_at: i64,
    deleted_at: Option<i64>,
}

const WORKSPACE_ARTIFACT_COLS: &str =
    "id, project_id, current_path, kind, mime, current_digest, current_size, \
     current_mtime, created_at, updated_at, deleted_at";

/// Parity: `getWorkspaceArtifact` (chat-artifacts/store.ts:379).
fn get_workspace_artifact(
    store: &Store,
    id: &str,
) -> Result<Option<WorkspaceArtifactRow>, StoreError> {
    store.query_one(
        &format!(
            "SELECT {WORKSPACE_ARTIFACT_COLS} FROM workspace_artifacts WHERE id = ?1"
        ),
        [id],
        |row| {
            Ok(WorkspaceArtifactRow {
                id: row.get("id")?,
                project_id: row.get("project_id")?,
                current_path: row.get("current_path")?,
                kind: row.get("kind")?,
                mime: row.get("mime")?,
                current_digest: row.get("current_digest")?,
                current_size: row.get("current_size")?,
                current_mtime: row.get("current_mtime")?,
                created_at: row.get("created_at")?,
                updated_at: row.get("updated_at")?,
                deleted_at: row.get("deleted_at")?,
            })
        },
    )
}

// ---- metadata projections --------------------------------------------------

/// JS truthiness for the `if (row.x)` gates over nullable text columns.
fn truthy_str(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|value| !value.is_empty())
}

/// Parity: `snapshotMetadata` (chat-artifacts.ts:251) — the six always-on
/// fields, the truthy optionals, `readyAt` by presence (`!== null`), and
/// `byteSize` looked up through `contentDigest ?? thumbnailDigest`.
fn snapshot_metadata(store: &Store, snapshot: &SnapshotRow) -> Result<Value, StoreError> {
    let mut object = Map::new();
    object.insert("id".to_string(), json!(snapshot.id));
    object.insert("projectId".to_string(), json!(snapshot.project_id));
    object.insert(
        "sourcePathAtCapture".to_string(),
        json!(snapshot.source_path_at_capture),
    );
    object.insert("kind".to_string(), json!(snapshot.kind));
    object.insert("state".to_string(), json!(snapshot.capture_state));
    object.insert("createdAt".to_string(), json!(snapshot.created_at));

    for (key, value) in [
        ("workspaceArtifactId", &snapshot.workspace_artifact_id),
        ("mime", &snapshot.mime),
        ("contentDigest", &snapshot.content_digest),
        ("thumbnailDigest", &snapshot.thumbnail_digest),
        ("failureCode", &snapshot.failure_code),
        ("runId", &snapshot.run_id),
        ("mediaTaskId", &snapshot.media_task_id),
    ] {
        if let Some(value) = truthy_str(value) {
            object.insert(key.to_string(), json!(value));
        }
    }
    if let Some(ready_at) = snapshot.ready_at {
        object.insert("readyAt".to_string(), json!(ready_at));
    }

    // `contentDigest ?? thumbnailDigest`, then the same truthiness gate — an
    // empty-string digest blocks the thumbnail fallback, exactly like JS.
    let digest = snapshot
        .content_digest
        .clone()
        .or_else(|| snapshot.thumbnail_digest.clone());
    if let Some(digest) = digest.filter(|value| !value.is_empty()) {
        if let Some(blob) = get_blob(store, &digest)? {
            object.insert("byteSize".to_string(), json!(blob.byte_size));
        }
    }
    Ok(Value::Object(object))
}

/// Parity: the `metadata` object literal in the workspace route
/// (chat-artifacts.ts:159) — `deleted` is the tombstone flag, `currentPath`
/// is present-but-null once the file is gone, and the size/mtime pairs are
/// included by presence (`!== null`), not truthiness.
fn workspace_artifact_metadata(artifact: &WorkspaceArtifactRow) -> Value {
    let mut object = Map::new();
    object.insert("id".to_string(), json!(artifact.id));
    object.insert("projectId".to_string(), json!(artifact.project_id));
    object.insert("currentPath".to_string(), json!(artifact.current_path));
    object.insert("kind".to_string(), json!(artifact.kind));
    object.insert(
        "deleted".to_string(),
        json!(artifact.deleted_at.is_some()),
    );
    object.insert("createdAt".to_string(), json!(artifact.created_at));
    object.insert("updatedAt".to_string(), json!(artifact.updated_at));
    if let Some(value) = truthy_str(&artifact.mime) {
        object.insert("mime".to_string(), json!(value));
    }
    if let Some(value) = truthy_str(&artifact.current_digest) {
        object.insert("currentDigest".to_string(), json!(value));
    }
    if let Some(value) = artifact.current_size {
        object.insert("currentSize".to_string(), json!(value));
    }
    if let Some(value) = artifact.current_mtime {
        object.insert("currentMtime".to_string(), json!(value));
    }
    Value::Object(object)
}

// ---- message refs (chat-artifacts/refs.ts) ---------------------------------

/// Parity: `MESSAGE_ARTIFACT_COLS` (chat-artifacts/store.ts:115). This
/// projection is duplicated from `conversations.rs` (whose copy is private)
/// because both routes answer from the same rows.
const MESSAGE_ARTIFACT_COLS: &str = "id, message_id AS messageId, ordinal, \
     snapshot_id AS snapshotId, \
     workspace_artifact_id AS workspaceArtifactId, \
     display_policy AS displayPolicy, label_at_capture AS labelAtCapture, \
     kind, html_version_id AS htmlVersionId, created_at AS createdAt";

fn message_refs(store: &Store, project_id: &str, message_id: &str) -> Result<Vec<Value>, StoreError> {
    let rows = store.query(
        &format!(
            "SELECT {MESSAGE_ARTIFACT_COLS} FROM message_artifacts
              WHERE message_id = ?1 ORDER BY ordinal ASC"
        ),
        [message_id],
        artifact_row,
    )?;
    rows.iter().map(|row| to_ref(store, project_id, row)).collect()
}

fn artifact_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Map<String, Value>> {
    let mut out = Map::new();
    for name in [
        "id",
        "messageId",
        "ordinal",
        "snapshotId",
        "workspaceArtifactId",
        "displayPolicy",
        "labelAtCapture",
        "kind",
        "htmlVersionId",
        "createdAt",
    ] {
        out.insert(name.to_string(), column(row, name)?);
    }
    Ok(out)
}

/// One projected column, `undefined`-tolerant the way `better-sqlite3`
/// resolves a name that was never selected.
fn column(row: &rusqlite::Row<'_>, name: &str) -> rusqlite::Result<Value> {
    match row.get::<_, rusqlite::types::Value>(name) {
        Ok(rusqlite::types::Value::Null) => Ok(Value::Null),
        Ok(rusqlite::types::Value::Integer(value)) => Ok(json!(value)),
        Ok(rusqlite::types::Value::Real(value)) => Ok(
            serde_json::Number::from_f64(value)
                .map(Value::Number)
                .unwrap_or(Value::Null),
        ),
        Ok(rusqlite::types::Value::Text(value)) => Ok(json!(value)),
        Ok(rusqlite::types::Value::Blob(value)) => {
            Ok(json!({ "type": "Buffer", "data": value }))
        }
        Err(rusqlite::Error::InvalidColumnName(_)) => Ok(Value::Null),
        Err(err) => Err(err),
    }
}

/// Parity: `toRef` (chat-artifacts/refs.ts:64) — only ids, labels and route
/// URLs cross this boundary; a URL is emitted once its bytes are ready.
fn to_ref(store: &Store, project_id: &str, row: &Map<String, Value>) -> Result<Value, StoreError> {
    let snapshot_id = row
        .get("snapshotId")
        .and_then(Value::as_str)
        .map(str::to_string);
    let snapshot = match &snapshot_id {
        Some(snapshot_id) => get_snapshot(store, snapshot_id)?,
        None => None,
    };

    let mut object = Map::new();
    object.insert("id".to_string(), row.get("id").cloned().unwrap_or(Value::Null));
    object.insert(
        "label".to_string(),
        row.get("labelAtCapture").cloned().unwrap_or(Value::Null),
    );
    object.insert(
        "kind".to_string(),
        json!(project_file_kind(row.get("kind").and_then(Value::as_str))),
    );
    object.insert(
        "displayPolicy".to_string(),
        row.get("displayPolicy").cloned().unwrap_or(Value::Null),
    );
    object.insert(
        "snapshotState".to_string(),
        json!(ref_state(snapshot.as_ref())),
    );
    if let Some(workspace_artifact_id) = row
        .get("workspaceArtifactId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        object.insert(
            "workspaceArtifactId".to_string(),
            json!(workspace_artifact_id),
        );
    }

    let ready = snapshot
        .as_ref()
        .is_some_and(|snapshot| snapshot.capture_state == "ready");
    if !ready {
        return Ok(Value::Object(object));
    }
    let snapshot = snapshot.expect("checked above");
    object.insert("snapshotId".to_string(), json!(snapshot.id));
    if truthy_str(&snapshot.content_digest).is_some() {
        object.insert(
            "snapshotUrl".to_string(),
            json!(chat_artifact_snapshot_content_url(
                project_id,
                &snapshot.id
            )),
        );
    }
    if truthy_str(&snapshot.thumbnail_digest).is_some() {
        object.insert(
            "thumbnailUrl".to_string(),
            json!(chat_artifact_snapshot_thumbnail_url(
                project_id,
                &snapshot.id
            )),
        );
    }
    Ok(Value::Object(object))
}

/// Parity: `refState` (chat-artifacts/refs.ts:95) — a ref with no snapshot
/// row never had a capture attempted: `legacy_unavailable`, not `failed`.
fn ref_state(snapshot: Option<&SnapshotRow>) -> &'static str {
    let Some(snapshot) = snapshot else {
        return "legacy_unavailable";
    };
    match snapshot.capture_state.as_str() {
        "ready" => "ready",
        "pending" => "pending",
        _ => "failed",
    }
}

/// Parity: `projectFileKind` (chat-artifacts/refs.ts:102).
fn project_file_kind(kind: Option<&str>) -> &'static str {
    const PROJECT_FILE_KINDS: [&str; 12] = [
        "html", "image", "video", "audio", "sketch", "text", "code", "pdf", "document",
        "presentation", "spreadsheet", "binary",
    ];
    match kind {
        Some(kind) => PROJECT_FILE_KINDS
            .iter()
            .find(|candidate| **candidate == kind)
            .copied()
            .unwrap_or("binary"),
        None => "binary",
    }
}

fn chat_artifact_snapshot_content_url(project_id: &str, snapshot_id: &str) -> String {
    format!(
        "/api/projects/{}/chat-artifact-snapshots/{}/content",
        uri_component(project_id),
        uri_component(snapshot_id)
    )
}

fn chat_artifact_snapshot_thumbnail_url(project_id: &str, snapshot_id: &str) -> String {
    format!(
        "/api/projects/{}/chat-artifact-snapshots/{}/thumbnail",
        uri_component(project_id),
        uri_component(snapshot_id)
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

// ---- response shaping ------------------------------------------------------

/// Parity: `INLINE_SAFE_MIME` (chat-artifacts.ts:41) — anchored, so a
/// parameterized type (`image/png; charset=…`) is never inline.
fn is_inline_safe_mime(mime: &str) -> bool {
    let lower = mime.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("image/") {
        return matches!(rest, "png" | "jpeg" | "gif" | "webp" | "avif");
    }
    for prefix in ["video/", "audio/"] {
        if let Some(rest) = lower.strip_prefix(prefix) {
            return !rest.is_empty()
                && rest
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'+' | b'-'));
        }
    }
    false
}

/// Parity: `safeFilename` (chat-artifacts.ts:282) — capture-time basename,
/// stripped to a header-safe alphabet, capped at 120 characters.
fn safe_filename(source_path_at_capture: &str) -> String {
    let base = source_path_at_capture.rsplit('/').next().unwrap_or("snapshot");
    let mapped: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let out: String = mapped.chars().take(120).collect();
    if out.is_empty() {
        "snapshot".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_key_shapes_match_the_typescript_regexes() {
        let digest_hex = "a".repeat(64);
        let object_key = format!("objects/ab/cd/{digest_hex}");
        assert!(is_valid_storage_key(&object_key));
        assert!(!is_valid_storage_key(&object_key.replace("ab", "AB")));
        assert!(!is_valid_storage_key(&format!("objects/ab/cd/{}", "a".repeat(63))));
        assert!(!is_valid_storage_key("objects/ab/cd/../.."));
        assert!(is_valid_storage_key("tmp/123e4567-e89b-12d3-a456-426614174000.part"));
        assert!(!is_valid_storage_key("tmp/123e4567-e89b-12d3-a456-426614174000.part.bak"));
        assert!(!is_valid_storage_key("/etc/passwd"));
        assert!(!is_valid_storage_key(""));
    }

    #[test]
    fn storage_key_resolution_stays_in_the_blob_root() {
        let data = std::env::temp_dir().join("od-chat-artifact-key-test");
        let key = format!("objects/ab/cd/{}", "0".repeat(64));
        let resolved = resolve_storage_key(&data, &key).expect("valid key");
        assert_eq!(resolved, blob_root(&data).join(&key));
        assert!(resolve_storage_key(&data, "../../etc/passwd").is_none());
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn inline_mime_matches_the_anchored_typescript_regex() {
        for mime in [
            "image/png",
            "image/jpeg",
            "image/WEBP",
            "video/mp4",
            "audio/mpeg",
            "video/x-matroska",
        ] {
            assert!(is_inline_safe_mime(mime), "{mime} should be inline");
        }
        for mime in [
            "image/svg+xml",
            "text/html",
            "image/png; charset=utf-8",
            "image/apng",
            "video/",
            "application/pdf",
        ] {
            assert!(!is_inline_safe_mime(mime), "{mime} must not be inline");
        }
    }

    #[test]
    fn safe_filename_is_a_header_safe_basename() {
        assert_eq!(safe_filename("Design Files/hero image.png"), "hero_image.png");
        assert_eq!(safe_filename("a/b/"), "snapshot");
        assert_eq!(safe_filename(""), "snapshot");
        assert_eq!(safe_filename("/only-name.html"), "only-name.html");
        let long = format!("{}/{}", "x".repeat(200), "n".repeat(200));
        assert_eq!(safe_filename(&long).len(), 120);
    }

    #[test]
    fn team_mirror_revocation_follows_json_truthiness() {
        assert!(team_mirror_revoked(Some(r#"{"teamMirrorRevokedAt":1}"#)));
        assert!(team_mirror_revoked(Some(r#"{"teamMirrorRevokedAt":"2026-01-01"}"#)));
        assert!(!team_mirror_revoked(Some(r#"{"teamMirrorRevokedAt":null}"#)));
        assert!(!team_mirror_revoked(Some(r#"{"teamMirrorRevokedAt":false}"#)));
        assert!(!team_mirror_revoked(Some(r#"{"teamMirrorRevokedAt":0}"#)));
        assert!(!team_mirror_revoked(Some(r#"{"teamMirrorRevokedAt":""}"#)));
        assert!(!team_mirror_revoked(Some(r#"{"other":1}"#)));
        assert!(!team_mirror_revoked(Some("not json")));
        assert!(!team_mirror_revoked(None));
    }
}
