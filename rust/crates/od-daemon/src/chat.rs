//! Chat + BYOK route support — Rust parity for `apps/daemon/src/routes/chat.ts`.
//!
//! Initial surface: `POST /api/runs/:id/feedback`. Heavy BYOK proxy/
//! provider-model/connection-test surfaces remain future work.

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::routes::{api_error, AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/runs/{id}/feedback", post(post_feedback))
        .route("/api/provider/models", post(post_provider_models))
        .route("/api/test/connection", post(post_test_connection))
        .route(
            "/api/projects/{projectId}/critique/{runId}/interrupt",
            post(critique_not_implemented),
        )
        .route(
            "/api/projects/{projectId}/critique/{runId}/artifact",
            get(critique_not_implemented),
        )
        .route("/api/proxy/anthropic/stream", post(proxy_stream_not_implemented))
        .route("/api/proxy/openai/stream", post(proxy_stream_not_implemented))
        .route("/api/proxy/azure/stream", post(proxy_stream_not_implemented))
        .route("/api/proxy/google/stream", post(proxy_stream_not_implemented))
        .route("/api/proxy/ollama/stream", post(proxy_stream_not_implemented))
        .route("/api/proxy/{provider}/stream", post(proxy_stream_not_implemented))
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



const PROVIDER_PROTOCOLS: &[&str] = &[
    "anthropic", "openai", "azure", "google", "ollama", "senseaudio", "aihubmix", "bedrock",
];

async fn post_provider_models(
    Json(body): Json<Value>,
) -> Response {
    let body = body.as_object().cloned().unwrap_or_default();
    let protocol = body.get("protocol").and_then(Value::as_str).unwrap_or_default();
    if !PROVIDER_PROTOCOLS.contains(&protocol) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
            "protocol must be one of anthropic|openai|azure|google|ollama|senseaudio|aihubmix|bedrock",
        );
    }
    let body_value = Value::Object(body.clone());
    let base_url = body.get("baseUrl").and_then(Value::as_str).unwrap_or_default().trim();
    let api_key = body.get("apiKey").and_then(Value::as_str).unwrap_or_default().trim();
    let api_key_required = protocol != "aihubmix" && protocol != "bedrock";
    if base_url.is_empty() || (api_key_required && api_key.is_empty()) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
            if api_key_required {
                "baseUrl and apiKey are required"
            } else {
                "baseUrl is required"
            },
        );
    }
    let result = crate::provider_models::list_provider_models(&body_value).await;
    Json(result).into_response()
}

async fn post_test_connection(
    Json(body): Json<Value>,
) -> Response {
    let body = body.as_object().cloned().unwrap_or_default();
    let mode = body.get("mode").and_then(Value::as_str).unwrap_or_default();
    match mode {
        "provider" => {
            let protocol = body.get("protocol").and_then(Value::as_str).unwrap_or_default();
            if !PROVIDER_PROTOCOLS.contains(&protocol) {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "BAD_REQUEST",
                    "protocol must be one of anthropic|openai|azure|google|ollama|senseaudio|aihubmix|bedrock",
                );
            }
            let base_url = body.get("baseUrl").and_then(Value::as_str).unwrap_or_default().trim();
            let api_key = body.get("apiKey").and_then(Value::as_str).unwrap_or_default().trim();
            let model = body.get("model").and_then(Value::as_str).unwrap_or_default().trim();
            let api_key_required = protocol != "bedrock";
            if base_url.is_empty()
                || (api_key_required && api_key.is_empty())
                || model.is_empty()
            {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "BAD_REQUEST",
                    if api_key_required {
                        "baseUrl, apiKey, and model are required"
                    } else {
                        "baseUrl and model are required"
                    },
                );
            }
            api_error(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL", "Connection test failed")
        }
        "agent" => {
            let agent_id = body.get("agentId").and_then(Value::as_str).unwrap_or_default().trim();
            if agent_id.is_empty() {
                return api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", "agentId is required");
            }
            api_error(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL", "Agent test failed")
        }
        _ => api_error(StatusCode::BAD_REQUEST, "BAD_REQUEST", "mode must be one of provider|agent"),
    }
}



async fn proxy_stream_not_implemented() -> Response {
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "INTERNAL",
        "BYOK proxy streaming is not yet ported to the Rust daemon",
    )
}

async fn critique_not_implemented() -> Response {
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "INTERNAL",
        "Critique handlers are not yet ported to the Rust daemon",
    )
}

#[cfg(test)]
mod tests {}
