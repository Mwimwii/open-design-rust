//! Critique Theater endpoints — Rust parity port of
//! `apps/daemon/src/critique/interrupt-handler.ts` and
//! `apps/daemon/src/critique/artifact-handler.ts`.
//!
//! There is no live orchestrator registry in this daemon, so the interrupt
//! route always takes the `no_live_handle` recovery path for `running` rows
//! (parity with the TypeScript branch of the same name).

use std::path::{Path, PathBuf};

use axum::extract::{Path as AxumPath, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;

use crate::routes::{api_error, store_error_response, AppState};

/// Response size cap when the deployment does not configure one (parity:
/// `DEFAULT_RESPONSE_SIZE_CAP_BYTES`).
const DEFAULT_RESPONSE_SIZE_CAP_BYTES: u64 = 4 * 1024 * 1024;

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/projects/{projectId}/critique/{runId}/interrupt",
            post(post_interrupt),
        )
        .route(
            "/api/projects/{projectId}/critique/{runId}/artifact",
            get(get_artifact),
        )
}

struct CritiqueRunRow {
    project_id: String,
    status: String,
    artifact_path: Option<String>,
}

fn get_critique_run(
    state: &AppState,
    id: &str,
) -> Result<Option<CritiqueRunRow>, Box<Response>> {
    match state.store.query_one(
        "SELECT project_id, status, artifact_path FROM critique_runs WHERE id = ?1",
        [id],
        |row| {
            Ok(CritiqueRunRow {
                project_id: row.get("project_id")?,
                status: row.get("status")?,
                artifact_path: row.get("artifact_path")?,
            })
        },
    ) {
        Ok(row) => Ok(row),
        Err(err) => Err(Box::new(store_error_response(&err))),
    }
}

async fn post_interrupt(
    State(state): State<AppState>,
    AxumPath((project_id, run_id)): AxumPath<(String, String)>,
) -> Response {
    let project_id = project_id.trim().to_string();
    let run_id = run_id.trim().to_string();
    if project_id.is_empty() || run_id.is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
            "projectId and runId are required",
        );
    }

    let row = match get_critique_run(&state, &run_id) {
        Ok(row) => row,
        Err(response) => return *response,
    };
    let Some(row) = row.filter(|row| row.project_id == project_id) else {
        return api_error(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            "critique run not found",
        );
    };

    if row.status == "interrupted" {
        return (
            StatusCode::ACCEPTED,
            Json(json!({
                "runId": run_id,
                "accepted": true,
                "prevStatus": "interrupted",
            })),
        )
            .into_response();
    }

    if row.status != "running" {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": {
                    "code": "CONFLICT",
                    "message": format!("run is already in terminal status: {}", row.status),
                    "currentStatus": row.status,
                },
            })),
        )
            .into_response();
    }

    // No live orchestrator registry exists in this daemon, so the
    // `no_live_handle` recovery path is the only one: flip the row to
    // `interrupted` directly, mirroring `markRunInterruptedRecovery`.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let recovered = state
        .store
        .execute(
            "UPDATE critique_runs SET status = 'interrupted', updated_at = ?1 WHERE id = ?2 AND status = 'running'",
            rusqlite::params![now_ms, run_id],
        )
        .map(|changed| changed > 0)
        .unwrap_or(false);
    // Rounds payload parity: wrap existing rounds with the recovery
    // reason when the row flipped. Read-modify-write is best-effort;
    // the status flip above is the load-bearing write.
    if recovered {
        let _ = state.store.execute(
            "UPDATE critique_runs SET rounds_json = CASE WHEN json_type(rounds_json) = 'array' THEN json_object('rounds', json(rounds_json), 'recoveryReason', 'no_live_handle') ELSE rounds_json END WHERE id = ?1",
            rusqlite::params![run_id],
        );
    }
    let mut body = json!({
        "runId": run_id,
        "accepted": true,
        "prevStatus": "running",
        "recovered": true,
    });
    if !recovered {
        body["recoveryFailed"] = json!(true);
    }
    (StatusCode::ACCEPTED, Json(body)).into_response()
}

async fn get_artifact(
    State(state): State<AppState>,
    AxumPath((project_id, run_id)): AxumPath<(String, String)>,
) -> Response {
    let project_id = project_id.trim().to_string();
    let run_id = run_id.trim().to_string();
    if project_id.is_empty() || run_id.is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
            "projectId and runId are required",
        );
    }

    let row = match get_critique_run(&state, &run_id) {
        Ok(row) => row,
        Err(response) => return *response,
    };
    let Some(row) = row.filter(|row| row.project_id == project_id) else {
        return api_error(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            "critique run not found",
        );
    };

    let Some(artifact_path) = row.artifact_path.filter(|p| !p.is_empty()) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": {
                    "code": "NOT_FOUND",
                    "message": "critique run has no persisted artifact",
                    "currentStatus": row.status,
                },
            })),
        )
            .into_response();
    };

    let artifacts_root = critique_artifacts_root(state.config.paths.data_dir());
    let artifacts_root = artifacts_root
        .canonicalize()
        .unwrap_or(artifacts_root);
    let resolved = Path::new(&artifact_path);
    let resolved = if resolved.is_absolute() {
        resolved.to_path_buf()
    } else {
        artifacts_root.join(resolved)
    };
    // Path-traversal guard + symlink refusal, mirroring the TS handler.
    let within_root = resolved
        .canonicalize()
        .ok()
        .filter(|canonical| canonical.starts_with(&artifacts_root))
        .and_then(|canonical| {
            std::fs::symlink_metadata(&canonical)
                .ok()
                .filter(|meta| meta.is_file())
                .map(|_| canonical)
        });
    let Some(canonical) = within_root else {
        return api_error(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            "critique artifact not found",
        );
    };
    let bytes = match std::fs::read(&canonical) {
        Ok(bytes) => bytes,
        Err(_) => {
            return api_error(
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
                "critique artifact not found",
            );
        }
    };
    if bytes.len() as u64 > DEFAULT_RESPONSE_SIZE_CAP_BYTES {
        return api_error(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            "critique artifact exceeds response cap",
        );
    }
    let content_type = mime_for_extension(
        canonical
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or_default(),
    );
    let mut response = (StatusCode::OK, bytes).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, content_type.parse().unwrap());
    headers.insert("x-content-type-options", "nosniff".parse().unwrap());
    if content_type == "image/svg+xml" || content_type == "text/html" {
        headers.insert(
            "content-security-policy",
            "default-src 'none'; img-src data:; style-src 'unsafe-inline'"
                .parse()
                .unwrap(),
        );
    }
    headers.insert(
        header::CACHE_CONTROL,
        "private, max-age=3600".parse().unwrap(),
    );
    response
}

fn critique_artifacts_root(data_dir: &Path) -> PathBuf {
    data_dir.join("critique-artifacts")
}

fn mime_for_extension(extension: &str) -> &'static str {
    match extension.to_lowercase().as_str() {
        "html" => "text/html",
        "css" => "text/css",
        "md" => "text/markdown",
        "txt" => "text/plain",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        _ => "application/octet-stream",
    }
}
