//! Chat + BYOK route support — Rust parity for `apps/daemon/src/routes/chat.ts`.
//!
//! Initial surface: `POST /api/runs/:id/feedback`. Heavy BYOK proxy/
//! provider-model/connection-test surfaces remain future work.

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::routes::{api_error, AppState};

pub fn router() -> Router<AppState> {
    Router::new().route("/api/runs/{id}/feedback", post(post_feedback))
}

async fn post_feedback(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    if id.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "INVALID_RUN_ID", "runId missing");
    }
    let Some(body_obj) = body.as_object() else {
        // TS treats req.body || {} — non-object becomes {}. A scalar top-level
        // value still reaches the checks below as "no rating".
        // Fall through with empty.
        return handle_feedback(&state, &id, &Value::Object(serde_json::Map::new())).await;
    };
    handle_feedback(&state, &id, &Value::Object(body_obj.clone())).await
}

async fn handle_feedback(state: &AppState, id: &str, body: &Value) -> Response {
    for field in ["projectId", "conversationId", "assistantMessageId"] {
        if body.get(field).is_some() {
            return api_error(
                StatusCode::BAD_REQUEST,
                "INVALID_FEEDBACK_CONTEXT",
                "feedback project, conversation, and message identity are derived from the run",
            );
        }
    }
    let rating = body.get("rating").and_then(Value::as_str).unwrap_or_default();
    if rating != "positive" && rating != "negative" {
        return api_error(
            StatusCode::BAD_REQUEST,
            "INVALID_RATING",
            "rating must be positive or negative",
        );
    }
    let project_id = match state.runs.run_project_id(id) {
        Some(p) if !p.trim().is_empty() => p,
        _ => return api_error(StatusCode::NOT_FOUND, "NOT_FOUND", "run not found"),
    };
    let _ = project_id;
    // No telemetry sink / Langfuse bridge is ported yet; honestly report skipped.
    Json(json!({ "status": "skipped_no_sink" })).into_response()
}

#[cfg(test)]
mod tests {}
