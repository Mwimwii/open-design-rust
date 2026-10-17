//! The diagnostics export bundle (parity: `apps/daemon/src/diagnostics-export.ts`
//! plus `packages/diagnostics/src/*`).
//!
//! `GET /api/diagnostics/export` streams a zip of daemon logs, agent-CLI logs,
//! per-run event captures, crash reports and live snapshots back to the caller.
//! The web app's Export logs button falls back to this endpoint in a plain
//! browser tab (the desktop bridge is the other path), so a missing route
//! breaks the feature outside the packaged shell too.
//!
//! `POST /api/diagnostics/chat-scroll-forensics` is the rendezvous the renderer
//! uses to hand its chat-scroll capture to whichever export path runs next.

pub mod agent_logs;
pub mod browser_use;
pub mod evidence;
pub mod export;
pub mod manifest;
pub mod redaction;
pub mod sources;
pub mod time;
pub mod zip;

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, MatchedPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::routes::AppState;

pub use agent_logs::{build_agent_cli_log_sources, build_run_event_log_sources, AgentCliLogOptions};
pub use evidence::{
    daemon_health_paths, diagnostic_route_template, diagnostics_evidence_paths,
    recent_api_failures_summary, DiagnosticsEvidence, DiagnosticsState, FailureError, FailureInput,
    RecentApiFailure, CHAT_SCROLL_FORENSICS_PATH,
};
pub use export::{
    create_diagnostics_export, DiagnosticsExportOptions, DiagnosticsRuntime,
    DIAGNOSTICS_CONTENT_TYPE, DIAGNOSTICS_EXPORT_PATH, DIAGNOSTICS_FILENAME_PREFIX,
};
pub use manifest::{DiagnosticsAppInfo, DiagnosticsContext, DiagnosticsManifest, MachineInfo};
pub use sources::{CollectedFile, LogSource, LogSourceKind};
pub use zip::{build_diagnostics_zip, DiagnosticsExportInput, DiagnosticsExportResult};

/// Body ceiling for a chat-scroll capture (parity:
/// `CHAT_SCROLL_FORENSICS_BODY_LIMIT`). A capture carries the chat log's full
/// `outerHTML`, and a long transcript runs to several megabytes.
pub const CHAT_SCROLL_FORENSICS_BODY_LIMIT: usize = 24 * 1024 * 1024;

pub fn router() -> Router<AppState> {
    Router::new()
        .route(DIAGNOSTICS_EXPORT_PATH, get(export_handler))
        .route(
            CHAT_SCROLL_FORENSICS_PATH,
            post(chat_scroll_handler).layer(DefaultBodyLimit::max(CHAT_SCROLL_FORENSICS_BODY_LIMIT)),
        )
}

async fn export_handler(State(state): State<AppState>) -> Response {
    let data_dir = state.config.paths.data_dir().to_path_buf();
    let options = DiagnosticsExportOptions {
        // The Rust daemon has no sidecar runtime, so there is no shell log tree
        // to sweep: this is the standalone warning the TypeScript daemon emits
        // for a plain `od` launch.
        runtime: None,
        project_root: None,
        runs_dir: Some(state.config.paths.runs_dir()),
        data_dir: Some(data_dir),
        diagnostics: state.diagnostics.clone(),
        automatic_upload_status: None,
    };
    create_diagnostics_export(options).await
}

async fn chat_scroll_handler(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    if !body.is_object() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "INVALID_CHAT_SCROLL_FORENSICS",
                "message": "Expected a JSON object body.",
            })),
        )
            .into_response();
    }
    let received_at = state.diagnostics.chat_scroll.store(body);
    Json(json!({ "ok": true, "receivedAtIso": received_at })).into_response()
}

/// What the response status means for a journal entry, because the middleware
/// cannot see the JSON body the handler produced. The TypeScript journal takes
/// the call site's own `code` and `retryable` from `sendApiError`; the grouping
/// key the bundle really uses is the route template plus the status, which this
/// preserves exactly.
pub fn status_code_token(status: u16) -> &'static str {
    match status {
        400 => "BAD_REQUEST",
        401 => "UNAUTHORIZED",
        403 => "FORBIDDEN",
        404 => "NOT_FOUND",
        405 => "METHOD_NOT_ALLOWED",
        409 => "CONFLICT",
        413 => "PAYLOAD_TOO_LARGE",
        422 => "INVALID_REQUEST",
        429 => "TOO_MANY_REQUESTS",
        500 => "INTERNAL",
        501 => "NOT_IMPLEMENTED",
        503 => "UNAVAILABLE",
        504 => "TIMEOUT",
        other if other >= 500 => "SERVER_ERROR",
        _ => "REQUEST_FAILED",
    }
}

/// Map the router's own catch-all pattern onto the TypeScript 404 marker.
fn matched_route_template(matched_path: Option<&str>) -> String {
    match matched_path {
        Some("/api/{*rest}") | None | Some("") => "/api/:unmatched".to_string(),
        other => diagnostic_route_template(other),
    }
}

/// Layer that journals every failed `/api` response, feeding both
/// `summary/recent-api-failures.json` and the environment-evidence failure
/// groups (parity: `sendApiError` → `recordApiFailure` → `recordDiagnosticFailure`).
pub async fn api_failure_recorder(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let matched = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_string());
    let method = request.method().to_string();
    let response = next.run(request).await;
    let status = response.status().as_u16();
    if status >= 400 {
        let path = matched_route_template(matched.as_deref());
        state.diagnostics.api_failures.record(RecentApiFailure {
            at: time::now_iso_millis(),
            method,
            path: path.clone(),
            status: i64::from(status),
            code: status_code_token(status).to_string(),
            retryable: status >= 500,
            request_id: None,
        });
        state.diagnostics.evidence.record(&FailureInput {
            source: "local-api".to_string(),
            operation: Some(path),
            status: Some(i64::from(status)),
            ..Default::default()
        });
    }
    response
}

/// The daemon's shared diagnostics store, held by [`AppState`].
pub type SharedDiagnostics = Arc<DiagnosticsState>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_templates_collapse_the_catch_all() {
        assert_eq!(matched_route_template(Some("/api/{*rest}")), "/api/:unmatched");
        assert_eq!(matched_route_template(None), "/api/:unmatched");
        assert_eq!(matched_route_template(Some("/api/test/connection")), "/api/test/connection");
    }

    #[test]
    fn status_tokens_cover_the_failure_classes() {
        assert_eq!(status_code_token(401), "UNAUTHORIZED");
        assert_eq!(status_code_token(404), "NOT_FOUND");
        assert_eq!(status_code_token(422), "INVALID_REQUEST");
        assert_eq!(status_code_token(500), "INTERNAL");
        assert_eq!(status_code_token(503), "UNAVAILABLE");
        assert_eq!(status_code_token(599), "SERVER_ERROR");
        assert_eq!(status_code_token(418), "REQUEST_FAILED");
    }
}
