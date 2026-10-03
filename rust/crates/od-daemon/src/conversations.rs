//! Project conversation routes — parity port of
//! `apps/daemon/src/routes/project/conversations.ts` (the six CRUD routes)
//! plus the `apps/daemon/src/db.ts` storage helpers those routes call.
//!
//! Documented gaps against the TypeScript daemon: the workspace-authority gate
//! (`authorizeProjectRequest`) always allows — its fixture fallback; brand
//! transcript backfill, `cancelRunsOwnedBy` (no runs engine yet), and
//! `ctx.telemetry.reportFinalizedMessage` (no telemetry outbox in this crate)
//! are skipped. Strategy-task turn decoration and chat artifact refs
//! (`message_artifacts`) are ported.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use axum::body::to_bytes;
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, put};
use axum::{Json, Router};
use serde_json::{json, Map, Value};

use crate::routes::{api_error, internal_error, store_error_response, AppState};
use crate::storage::{Store, StoreError};

/// Parity: `PROJECT_COMMENT_ANCHOR_PREFIX`.
const PROJECT_COMMENT_ANCHOR_PREFIX: &str = "comment-anchor-";

/// Parity: `TERMINAL_RUN_STATUSES`.
const TERMINAL_RUN_STATUSES: [&str; 3] = ["succeeded", "failed", "canceled"];

/// Parity: the daemon's global `express.json({ limit: '4mb' })`.
const BODY_LIMIT: usize = 4 * 1024 * 1024;

/// Parity: `getMessage`'s column list. `db.ts` omits `task_analytics_json`
/// here, so those reads never expose `taskAnalytics`.
const GET_MESSAGE_COLUMNS: &str = "id, role, content, agent_id AS agentId, \
     agent_name AS agentName, run_id AS runId, run_status AS runStatus, \
     result_delivery_state AS resultDeliveryState, \
     last_run_event_id AS lastRunEventId, events_json AS eventsJson, \
     attachments_json AS attachmentsJson, \
     comment_attachments_json AS commentAttachmentsJson, \
     produced_files_json AS producedFilesJson, \
     trace_object_files_json AS traceObjectFilesJson, feedback_json AS feedbackJson, \
     pre_turn_file_names_json AS preTurnFileNamesJson, session_mode AS sessionMode, \
     run_context_json AS runContextJson, \
     applied_plugin_snapshot_json AS appliedPluginSnapshotJson, \
     forked_into_json AS forkedIntoJson, cancel_origin AS cancelOrigin, \
     created_at AS createdAt, started_at AS startedAt, ended_at AS endedAt, position";

/// Parity: `listMessages` (and the `upsertMessage` re-read) column list —
/// `getMessage` is the one caller that drops `task_analytics_json`.
const LIST_MESSAGE_COLUMNS: &str = "id, role, content, agent_id AS agentId, \
     agent_name AS agentName, run_id AS runId, run_status AS runStatus, \
     result_delivery_state AS resultDeliveryState, \
     last_run_event_id AS lastRunEventId, events_json AS eventsJson, \
     attachments_json AS attachmentsJson, \
     comment_attachments_json AS commentAttachmentsJson, \
     produced_files_json AS producedFilesJson, \
     trace_object_files_json AS traceObjectFilesJson, feedback_json AS feedbackJson, \
     pre_turn_file_names_json AS preTurnFileNamesJson, session_mode AS sessionMode, \
     run_context_json AS runContextJson, task_analytics_json AS taskAnalyticsJson, \
     applied_plugin_snapshot_json AS appliedPluginSnapshotJson, \
     forked_into_json AS forkedIntoJson, cancel_origin AS cancelOrigin, \
     created_at AS createdAt, started_at AS startedAt, ended_at AS endedAt, position";

/// The conversation row columns `normalizeConversation` reads.
const CONVERSATION_COLUMNS: [&str; 12] = [
    "id",
    "projectId",
    "title",
    "sessionMode",
    "createdAt",
    "updatedAt",
    "messageCount",
    "latestRunStatus",
    "latestRunStartedAt",
    "latestRunEndedAt",
    "latestRunMessageId",
    "totalDurationMs",
];

/// Register the conversation routes (parity: `registerProjectConversationRoutes`).
pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/projects/{id}/conversations",
            get(list_conversations_handler).post(create_conversation_handler),
        )
        .route(
            "/api/projects/{id}/conversations/{cid}",
            patch(update_conversation_handler).delete(delete_conversation_handler),
        )
        .route(
            "/api/projects/{id}/conversations/{cid}/messages",
            get(list_messages_handler),
        )
        .route(
            "/api/projects/{id}/conversations/{cid}/messages/{mid}",
            put(put_message_handler),
        )
}

/// `Box<Response>` error helpers — mirrors `routes.rs`, which boxes its
/// `Response` errors so the `Err` variant stays small for clippy.
fn api_err(status: StatusCode, code: &str, message: &str) -> Box<Response> {
    Box::new(api_error(status, code, message))
}

fn store_err(err: &StoreError) -> Box<Response> {
    Box::new(store_error_response(err))
}

// ---- handlers -------------------------------------------------------------

/// `GET /api/projects/:id/conversations`.
async fn list_conversations_handler(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let store = state.store.clone();
    let project_id = id;
    let result =
        tokio::task::spawn_blocking(move || list_conversations_for_project(&store, &project_id))
            .await;
    match result {
        Ok(Ok(Some(conversations))) => Json(json!({ "conversations": conversations })).into_response(),
        Ok(Ok(None)) => api_error(StatusCode::NOT_FOUND, "PROJECT_NOT_FOUND", "project not found"),
        Ok(Err(err)) => store_error_response(&err),
        Err(join) => internal_error(&join.to_string()),
    }
}

/// `POST /api/projects/:id/conversations` — plain, fork, and seeded forks.
async fn create_conversation_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let body = match read_json_body(request).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    let store = state.store.clone();
    let project_id = id;
    let result = tokio::task::spawn_blocking(move || {
        create_conversation_blocking(&store, &project_id, &body)
    })
    .await;
    match result {
        Ok(Ok(conversation)) => Json(json!({ "conversation": conversation })).into_response(),
        Ok(Err(response)) => *response,
        Err(join) => internal_error(&join.to_string()),
    }
}

/// `PATCH /api/projects/:id/conversations/:cid`.
async fn update_conversation_handler(
    State(state): State<AppState>,
    Path((id, cid)): Path<(String, String)>,
    request: Request,
) -> Response {
    let body = match read_json_body(request).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    let store = state.store.clone();
    let project_id = id;
    let conversation_id = cid;
    let result = tokio::task::spawn_blocking(move || {
        update_conversation_blocking(&store, &project_id, &conversation_id, &body)
    })
    .await;
    match result {
        Ok(Ok(conversation)) => Json(json!({ "conversation": conversation })).into_response(),
        Ok(Err(response)) => *response,
        Err(join) => internal_error(&join.to_string()),
    }
}

/// `DELETE /api/projects/:id/conversations/:cid`.
async fn delete_conversation_handler(
    State(state): State<AppState>,
    Path((id, cid)): Path<(String, String)>,
) -> Response {
    let store = state.store.clone();
    let project_id = id;
    let conversation_id = cid;
    let result = tokio::task::spawn_blocking(move || {
        delete_conversation_blocking(&store, &project_id, &conversation_id)
    })
    .await;
    match result {
        Ok(Ok(())) => Json(json!({ "ok": true })).into_response(),
        Ok(Err(response)) => *response,
        Err(join) => internal_error(&join.to_string()),
    }
}

/// `GET /api/projects/:id/conversations/:cid/messages`.
async fn list_messages_handler(
    State(state): State<AppState>,
    Path((id, cid)): Path<(String, String)>,
) -> Response {
    let store = state.store.clone();
    let project_id = id;
    let conversation_id = cid;
    let result = tokio::task::spawn_blocking(move || {
        list_messages_blocking(&store, &project_id, &conversation_id)
    })
    .await;
    match result {
        Ok(Ok(messages)) => Json(json!({ "messages": messages })).into_response(),
        Ok(Err(response)) => *response,
        Err(join) => internal_error(&join.to_string()),
    }
}

/// `PUT /api/projects/:id/conversations/:cid/messages/:mid`.
async fn put_message_handler(
    State(state): State<AppState>,
    Path((id, cid, mid)): Path<(String, String, String)>,
    request: Request,
) -> Response {
    let body = match read_json_body(request).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    let store = state.store.clone();
    let project_id = id;
    let conversation_id = cid;
    let message_id = mid;
    let result = tokio::task::spawn_blocking(move || {
        put_message_blocking(&store, &project_id, &conversation_id, &message_id, &body)
    })
    .await;
    match result {
        Ok(Ok(message)) => Json(json!({ "message": message })).into_response(),
        Ok(Err(response)) => *response,
        Err(join) => internal_error(&join.to_string()),
    }
}

// ---- request plumbing -----------------------------------------------------

/// Express only parses bodies declared as JSON; anything else reaches the
/// handler with `req.body` unset, i.e. `{}` here. `express.json` runs in
/// `strict` mode: an empty body is `{}` and a non-object/array top level is a
/// parse failure.
async fn read_json_body(request: Request) -> Result<Value, Box<Response>> {
    if !is_json_content_type(request.headers()) {
        return Ok(Value::Object(Map::new()));
    }
    let bytes = match to_bytes(request.into_body(), BODY_LIMIT).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return Err(api_err(
                StatusCode::PAYLOAD_TOO_LARGE,
                "PAYLOAD_TOO_LARGE",
                "request entity too large",
            ));
        }
    };
    if bytes.is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let parsed: Value = serde_json::from_slice(&bytes).map_err(|_| {
        api_error(
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
            "invalid json body",
        )
    })?;
    if !parsed.is_object() && !parsed.is_array() {
        return Err(api_err(
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
            "invalid json body",
        ));
    }
    Ok(normalize_js_numbers(parsed))
}

fn is_json_content_type(headers: &HeaderMap) -> bool {
    let Some(value) = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let essence = value
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    essence == "application/json" || essence.ends_with("+json")
}

// ---- route bodies ---------------------------------------------------------

fn list_conversations_for_project(
    store: &Store,
    project_id: &str,
) -> Result<Option<Vec<Value>>, StoreError> {
    if store.get_project(project_id)?.is_none() {
        return Ok(None);
    }
    Ok(Some(list_conversations(store, project_id)?))
}

fn create_conversation_blocking(
    store: &Store,
    project_id: &str,
    body: &Value,
) -> Result<Value, Box<Response>> {
    if store
        .get_project(project_id)
        .map_err(|err| store_err(&err))?
        .is_none()
    {
        return Err(api_err(
            StatusCode::NOT_FOUND,
            "PROJECT_NOT_FOUND",
            "project not found",
        ));
    }
    let now = now_ms();
    let has_explicit_session_mode = body.as_object().is_some_and(|o| o.contains_key("sessionMode"));
    if has_explicit_session_mode
        && !is_chat_session_mode(body.get("sessionMode"))
    {
        return Err(api_err(
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
            "sessionMode must be one of design, chat, or plan",
        ));
    }

    let requested_fork_message_id = body
        .get("forkAfterMessageId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let source = match body
        .get("seedFromConversationId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        Some(source_id) => routable_conversation(store, project_id, source_id)
            .map_err(|err| store_err(&err))?,
        None => None,
    };
    let client_seed_messages = body.get("seedMessages").and_then(Value::as_array).map(|entries| {
        entries
            .iter()
            .filter(|entry| {
                !entry.is_null() && entry.get("role").and_then(Value::as_str).is_some()
            })
            .cloned()
            .collect::<Vec<Value>>()
    });
    let client_fork_fallback_message = body
        .get("forkFallbackMessage")
        .filter(|message| {
            message.get("id").and_then(Value::as_str).is_some()
                && message.get("role").and_then(Value::as_str).is_some()
                && message.get("content").and_then(Value::as_str).is_some()
        })
        .cloned();
    let client_fallback_predecessor = match body.get("forkFallbackPredecessorMessageId") {
        Some(Value::Null) => FallbackPredecessor::Empty,
        Some(Value::String(value)) if !value.is_empty() => {
            FallbackPredecessor::Id(value.clone())
        }
        _ => FallbackPredecessor::Missing,
    };

    let mut seed_messages: Vec<Value> = Vec::new();
    if let Some(client_seed) = client_seed_messages.filter(|seed| !seed.is_empty()) {
        seed_messages = client_seed;
        if let Some(fork_id) = requested_fork_message_id.as_deref() {
            if let Some(index) = seed_messages
                .iter()
                .position(|message| message.get("id").and_then(Value::as_str) == Some(fork_id))
            {
                seed_messages.truncate(index + 1);
            }
        }
    } else if let Some(source_conversation) = &source {
        seed_messages = list_messages(store, source_conversation["id"].as_str().unwrap_or(""))
            .map_err(|err| store_err(&err))?;
        if let Some(fork_id) = requested_fork_message_id.as_deref() {
            let fork_index = seed_messages
                .iter()
                .position(|message| message.get("id").and_then(Value::as_str) == Some(fork_id));
            match fork_index {
                None => {
                    let fallback_matches = client_fork_fallback_message
                        .as_ref()
                        .and_then(|message| message.get("id"))
                        .and_then(Value::as_str)
                        == Some(fork_id);
                    if !fallback_matches {
                        return Err(api_err(
                            StatusCode::NOT_FOUND,
                            "FORK_MESSAGE_NOT_FOUND",
                            "fork message not found",
                        ));
                    }
                    match client_fallback_predecessor {
                        FallbackPredecessor::Missing => {
                            return Err(api_err(
                                StatusCode::BAD_REQUEST,
                                "BAD_REQUEST",
                                "fork fallback predecessor is required",
                            ));
                        }
                        FallbackPredecessor::Empty => seed_messages.clear(),
                        FallbackPredecessor::Id(predecessor) => {
                            let predecessor_index = seed_messages.iter().position(|message| {
                                message.get("id").and_then(Value::as_str)
                                    == Some(predecessor.as_str())
                            });
                            match predecessor_index {
                                None => {
                                    return Err(api_err(
                                        StatusCode::NOT_FOUND,
                                        "FORK_FALLBACK_PREDECESSOR_NOT_FOUND",
                                        "fork fallback predecessor not found",
                                    ));
                                }
                                Some(index) => seed_messages.truncate(index + 1),
                            }
                        }
                    }
                    seed_messages.push(client_fork_fallback_message.clone().unwrap_or(Value::Null));
                }
                Some(index) => seed_messages.truncate(index + 1),
            }
        }
    } else if requested_fork_message_id.is_some() {
        return Err(api_err(
            StatusCode::NOT_FOUND,
            "FORK_SOURCE_NOT_FOUND",
            "fork source conversation not found",
        ));
    }

    let session_mode = if has_explicit_session_mode {
        normalize_conversation_session_mode(body.get("sessionMode"))
    } else if let Some(source_conversation) = &source {
        normalize_conversation_session_mode(source_conversation.get("sessionMode"))
    } else {
        "design"
    };
    let explicit_title = body
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string);
    let resolved_title = match explicit_title {
        Some(title) => Some(title),
        None => match &source {
            Some(source_conversation) => {
                let existing_titles = list_conversation_titles(store, project_id)
                    .map_err(|err| store_err(&err))?;
                next_forked_conversation_title(
                    source_conversation.get("title").and_then(Value::as_str),
                    &existing_titles,
                )
            }
            None => None,
        },
    };

    let conversation_id = random_id();
    let conversation = insert_conversation(
        store,
        &conversation_id,
        project_id,
        resolved_title
            .as_deref()
            .map(|title| Value::String(title.to_string()))
            .unwrap_or(Value::Null),
        session_mode,
        now,
    )
    .map_err(|err| store_err(&err))?;

    if !seed_messages.is_empty() {
        let boundary_at = seed_messages.len() - 1;
        let inherited_title = source
            .as_ref()
            .and_then(|conversation| conversation.get("title").and_then(Value::as_str))
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(str::to_string);
        for (index, message) in seed_messages.iter().enumerate() {
            let mut copy = message.as_object().cloned().unwrap_or_default();
            copy.insert("id".to_string(), Value::String(random_id()));
            copy.remove("runId");
            copy.remove("lastRunEventId");
            match message
                .get("runStatus")
                .and_then(Value::as_str)
                .filter(|status| TERMINAL_RUN_STATUSES.contains(status))
            {
                Some(status) => {
                    copy.insert("runStatus".to_string(), json!(status));
                }
                None => {
                    copy.remove("runStatus");
                }
            }
            match inherited_title.as_ref().filter(|_| index == boundary_at) {
                Some(title) => {
                    copy.insert(
                        "forkedInto".to_string(),
                        json!({
                            "title": title,
                            "conversationId": body
                                .get("seedFromConversationId")
                                .cloned()
                                .unwrap_or(Value::Null),
                        }),
                    );
                }
                None => {
                    copy.remove("forkedInto");
                }
            }
            upsert_message(store, &conversation_id, Value::Object(copy))
                .map_err(|err| store_err(&err))?;
        }
    }

    Ok(conversation)
}

enum FallbackPredecessor {
    Missing,
    Empty,
    Id(String),
}

fn update_conversation_blocking(
    store: &Store,
    project_id: &str,
    conversation_id: &str,
    body: &Value,
) -> Result<Value, Box<Response>> {
    let conversation = routable_conversation(store, project_id, conversation_id)
        .map_err(|err| store_err(&err))?;
    if conversation.is_none() {
        return Err(api_err(
            StatusCode::NOT_FOUND,
            "CONVERSATION_NOT_FOUND",
            "not found",
        ));
    }
    if body.as_object().is_some_and(|o| o.contains_key("sessionMode"))
        && !is_chat_session_mode(body.get("sessionMode"))
    {
        return Err(api_err(
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
            "sessionMode must be one of design, chat, or plan",
        ));
    }
    update_conversation(store, conversation_id, body)
        .map_err(|err| store_err(&err))
        .map(|updated| updated.unwrap_or(Value::Null))
}

fn delete_conversation_blocking(
    store: &Store,
    project_id: &str,
    conversation_id: &str,
) -> Result<(), Box<Response>> {
    let conversation = routable_conversation(store, project_id, conversation_id)
        .map_err(|err| store_err(&err))?;
    if conversation.is_none() {
        return Err(api_err(
            StatusCode::NOT_FOUND,
            "CONVERSATION_NOT_FOUND",
            "not found",
        ));
    }
    delete_conversation_and_repair_team_comment_anchor(store, project_id, conversation_id)
        .map_err(|err| store_err(&err))
}

fn list_messages_blocking(
    store: &Store,
    project_id: &str,
    conversation_id: &str,
) -> Result<Vec<Value>, Box<Response>> {
    let conversation = routable_conversation(store, project_id, conversation_id)
        .map_err(|err| store_err(&err))?;
    if conversation.is_none() {
        return Err(api_err(
            StatusCode::NOT_FOUND,
            "CONVERSATION_NOT_FOUND",
            "conversation not found",
        ));
    }
    let messages = list_messages(store, conversation_id)
        .map_err(|err| store_err(&err))?;
    // A Full Plan turn spans several physical Runs; the client needs each
    // message's logical-task position to render one turn (parity:
    // `strategyTaskTurnsForRunIds`).
    let run_ids: Vec<String> = messages
        .iter()
        .filter_map(|message| {
            message
                .get("runId")
                .and_then(Value::as_str)
                .filter(|run_id| !run_id.is_empty())
                .map(str::to_string)
        })
        .collect();
    let turns = strategy_task_turns_for_run_ids(store, &run_ids, project_id, conversation_id)
        .map_err(|err| store_err(&err))?;
    Ok(messages
        .into_iter()
        .map(|message| decorate_message_with_strategy_turn(message, &turns))
        .collect())
}

fn put_message_blocking(
    store: &Store,
    project_id: &str,
    conversation_id: &str,
    message_id: &str,
    body: &Value,
) -> Result<Value, Box<Response>> {
    let conversation = routable_conversation(store, project_id, conversation_id)
        .map_err(|err| store_err(&err))?;
    if conversation.is_none() {
        return Err(api_err(
            StatusCode::NOT_FOUND,
            "CONVERSATION_NOT_FOUND",
            "conversation not found",
        ));
    }
    // `const m = req.body || {}` — falsy JSON bodies (null, 0, "", false) are
    // replaced with an empty object; truthy arrays/strings stay themselves.
    let empty = Value::Object(Map::new());
    let m: &Value = if js_truthy(body) { body } else { &empty };
    if js_truthy(m.get("id").unwrap_or(&Value::Null))
        && m.get("id") != Some(&Value::String(message_id.to_string()))
    {
        return Err(api_err(
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
            "id mismatch",
        ));
    }
    // Scope the stored lookup to the conversation authorized by the route. If
    // a message with this id exists in ANOTHER conversation, reject rather
    // than rewrite the wrong row through this endpoint.
    let existing = get_message(store, message_id, Some(conversation_id))
        .map_err(|err| store_err(&err))?;
    if existing.is_none()
        && get_message(store, message_id, None)
            .map_err(|err| store_err(&err))?
            .is_some()
    {
        return Err(api_err(
            StatusCode::NOT_FOUND,
            "MESSAGE_NOT_FOUND",
            "message not found",
        ));
    }
    if m.get("createOnly") == Some(&Value::Bool(true)) {
        if let Some(existing) = &existing {
            return Ok(existing.clone());
        }
    }

    // `Array.isArray(m.events) ? { ...m, events: compact } : m`
    let normalized = match m.get("events").and_then(Value::as_array) {
        Some(events) => {
            let compacted = compact_adjacent_message_agent_events(events.clone());
            let mut spread = Map::new();
            spread_json_value_into(&mut spread, m);
            spread.insert("events".to_string(), Value::Array(compacted));
            Value::Object(spread)
        }
        None => m.clone(),
    };
    let sibling_keys = list_sibling_run_done_keys(store, conversation_id, message_id)
        .map_err(|err| store_err(&err))?;
    let merged = merge_message_write_for_daemon_backed(
        existing.as_ref(),
        &normalized,
        &sibling_keys,
    );
    // `{ ...mergeMessageWriteForDaemonBacked(...), id: req.params.mid }`
    let mut final_message = Map::new();
    spread_json_value_into(&mut final_message, &merged);
    final_message.insert("id".to_string(), json!(message_id));
    let saved = upsert_message(store, conversation_id, Value::Object(final_message))
        .map_err(|err| store_err(&err))?;
    // Bump the parent project's updatedAt so the project list re-orders
    // (parity: `updateProject(db, projectId, {})`).
    bump_project_updated_at(store, project_id)
        .map_err(|err| store_err(&err))?;
    Ok(saved)
}

// ---- conversations ---------------------------------------------------------

/// Parity: `getRoutableConversation`.
fn routable_conversation(
    store: &Store,
    project_id: &str,
    conversation_id: &str,
) -> Result<Option<Value>, StoreError> {
    if conversation_id.starts_with(PROJECT_COMMENT_ANCHOR_PREFIX) {
        return Ok(None);
    }
    let conversation = get_conversation(store, conversation_id)?;
    Ok(conversation.filter(|conversation| {
        conversation.get("projectId").and_then(Value::as_str) == Some(project_id)
    }))
}

fn list_conversation_titles(store: &Store, project_id: &str) -> Result<Vec<Option<String>>, StoreError> {
    Ok(list_conversations(store, project_id)?
        .into_iter()
        .map(|conversation| {
            conversation
                .get("title")
                .and_then(|title| title.as_str().map(str::to_string))
        })
        .collect())
}

fn list_conversations(store: &Store, project_id: &str) -> Result<Vec<Value>, StoreError> {
    let sql = format!(
        "WITH project_conversations AS (
            SELECT id, project_id AS projectId, title, session_mode AS sessionMode,
                   created_at AS createdAt, updated_at AS updatedAt
              FROM conversations
             WHERE project_id = ?1
               AND id NOT LIKE 'comment-anchor-%'
         ),
         latest_runs AS (
            SELECT conversation_id AS conversationId,
                   run_status AS latestRunStatus,
                   started_at AS latestRunStartedAt,
                   ended_at AS latestRunEndedAt,
                   id AS latestRunMessageId
              FROM (
                SELECT m.conversation_id, m.run_status, m.started_at, m.ended_at, m.id,
                       ROW_NUMBER() OVER (
                         PARTITION BY m.conversation_id
                         ORDER BY m.position DESC
                       ) AS rn
                  FROM messages m
                  JOIN project_conversations c ON c.id = m.conversation_id
                 WHERE m.role = 'assistant'
                   AND m.run_status IS NOT NULL
              )
             WHERE rn = 1
         ),
         message_counts AS (
            SELECT m.conversation_id AS conversationId, COUNT(*) AS messageCount
              FROM messages m
              JOIN project_conversations c ON c.id = m.conversation_id
             GROUP BY m.conversation_id
         ),
         total_run_durations AS (
            SELECT m.conversation_id AS conversationId,
                   SUM({}) AS totalDurationMs
              FROM messages m
              JOIN project_conversations c ON c.id = m.conversation_id
             WHERE m.role = 'assistant'
               AND m.run_status IN ('succeeded', 'failed', 'canceled')
             GROUP BY m.conversation_id
         )
         SELECT c.id, c.projectId, c.title, c.sessionMode, c.createdAt, c.updatedAt,
                COALESCE(mc.messageCount, 0) AS messageCount,
                lr.latestRunStatus, lr.latestRunStartedAt,
                lr.latestRunEndedAt, lr.latestRunMessageId,
                trd.totalDurationMs
           FROM project_conversations c
           LEFT JOIN latest_runs lr ON lr.conversationId = c.id
           LEFT JOIN message_counts mc ON mc.conversationId = c.id
           LEFT JOIN total_run_durations trd ON trd.conversationId = c.id
          ORDER BY c.updatedAt DESC",
        terminal_run_duration_sql("m")
    );
    let mut rows = store.query(&sql, [project_id], conversation_row)?;
    attach_latest_run_events(store, &mut rows)?;
    Ok(rows
        .iter()
        .map(|row| Value::Object(normalize_conversation(row)))
        .collect::<Vec<Value>>())
}

/// Parity: `attachLatestRunEvents` — only rows whose timestamps cannot supply
/// a duration read their event log.
fn attach_latest_run_events(
    store: &Store,
    rows: &mut [Map<String, Value>],
) -> Result<(), StoreError> {
    let pending: Vec<String> = rows
        .iter()
        .filter(|row| {
            row.get("latestRunMessageId").is_some_and(is_non_null)
                && !has_both_timestamps(row)
        })
        .filter_map(|row| row.get("latestRunMessageId").and_then(Value::as_str).map(str::to_string))
        .collect();
    if pending.is_empty() {
        return Ok(());
    }
    for message_id in pending {
        let events = store.query_one(
            "SELECT events_json AS eventsJson FROM messages WHERE id = ?1",
            [message_id.as_str()],
            |row| optional_column(row, "eventsJson"),
        )?;
        let events = events.unwrap_or(Value::Null);
        for row in rows.iter_mut() {
            if row.get("latestRunMessageId").and_then(Value::as_str) == Some(message_id.as_str()) {
                row.insert("latestRunEventsJson".to_string(), events.clone());
            }
        }
    }
    Ok(())
}

/// Parity: `hasBothTimestamps` in `attachLatestRunEvents` — a null timestamp is
/// mapped to `undefined` before the finiteness check, so SQL NULL means "no
/// duration from timestamps".
fn has_both_timestamps(row: &Map<String, Value>) -> bool {
    let started = row.get("latestRunStartedAt").filter(|value| !value.is_null());
    let ended = row.get("latestRunEndedAt").filter(|value| !value.is_null());
    started.is_some_and(number_as_f64_finite) && ended.is_some_and(number_as_f64_finite)
}

fn is_non_null(value: &Value) -> bool {
    !value.is_null()
}

fn get_conversation(store: &Store, id: &str) -> Result<Option<Value>, StoreError> {
    let Some(row) = store.query_one(
        "SELECT id, project_id AS projectId, title, session_mode AS sessionMode, \
                created_at AS createdAt, updated_at AS updatedAt, \
                (SELECT COUNT(*) FROM messages WHERE conversation_id = conversations.id) AS messageCount \
           FROM conversations WHERE id = ?1",
        [id],
        conversation_row,
    )?
    else {
        return Ok(None);
    };
    // `{...normalizeConversation(r), latestRun: summary ?? undefined,
    // ...numberProperty('totalDurationMs', total)}` — the row itself carries no
    // run columns, so the summary queries below supply both overrides.
    let mut out = normalize_conversation(&row);

    let run_summary = store.query_one(
        "SELECT run_status AS runStatus, started_at AS startedAt, ended_at AS endedAt, \
                events_json AS eventsJson
           FROM messages
          WHERE conversation_id = ?1 AND role = 'assistant' AND run_status IS NOT NULL
          ORDER BY position DESC
          LIMIT 1",
        [id],
        |row| {
            let mut summary = Map::new();
            for name in ["runStatus", "startedAt", "endedAt", "eventsJson"] {
                summary.insert(name.to_string(), optional_column(row, name)?);
            }
            Ok(summary)
        },
    )?;
    match run_summary
        .as_ref()
        .and_then(conversation_run_summary_from_row)
    {
        Some(summary) => {
            out.insert("latestRun".to_string(), summary);
        }
        None => {
            out.remove("latestRun");
        }
    }

    let total = store.query_one(
        &format!(
            "SELECT SUM({}) AS totalDurationMs
               FROM messages
              WHERE conversation_id = ?1
                AND role = 'assistant'
                AND run_status IN ('succeeded', 'failed', 'canceled')",
            terminal_run_duration_sql("")
        ),
        [id],
        |row| optional_column(row, "totalDurationMs"),
    )?;
    match total
        .as_ref()
        .filter(|value| !value.is_null())
        .and_then(|value| value.as_f64())
        .filter(|value| value.is_finite())
    {
        Some(value) => {
            out.insert("totalDurationMs".to_string(), json!(value));
        }
        None => {
            out.remove("totalDurationMs");
        }
    }

    Ok(Some(Value::Object(out)))
}

fn insert_conversation(
    store: &Store,
    id: &str,
    project_id: &str,
    title: Value,
    session_mode: &str,
    now: i64,
) -> Result<Value, StoreError> {
    let title = sql_bind(&title)?;
    store.execute(
        "INSERT INTO conversations (id, project_id, title, session_mode, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![id, project_id, title, session_mode, now, now],
    )?;
    get_conversation(store, id).map(|conversation| conversation.unwrap_or(Value::Null))
}

fn update_conversation(
    store: &Store,
    id: &str,
    patch: &Value,
) -> Result<Option<Value>, StoreError> {
    let Some(existing) = get_conversation(store, id)? else {
        return Ok(None);
    };
    let patch_object = patch.as_object();
    let title = patch
        .get("title")
        .cloned()
        .or_else(|| existing.get("title").cloned())
        .unwrap_or(Value::Null);
    let session_mode = match patch_object {
        Some(object) if object.contains_key("sessionMode") => {
            normalize_conversation_session_mode(patch.get("sessionMode")).to_string()
        }
        _ => normalize_conversation_session_mode(existing.get("sessionMode")).to_string(),
    };
    let updated_at = match patch.get("updatedAt") {
        Some(Value::Number(number)) => json_f64(number.as_f64().unwrap_or(0.0)),
        _ => json!(now_ms()),
    };
    let title = sql_bind(&title)?;
    let updated_at = sql_bind(&updated_at)?;
    store.execute(
        "UPDATE conversations SET title = ?1, session_mode = ?2, updated_at = ?3 WHERE id = ?4",
        rusqlite::params![title, session_mode, updated_at, id],
    )?;
    get_conversation(store, id)
}

/// Port of `deleteConversationAndRepairTeamCommentAnchor`. The TypeScript
/// helper wraps the repair and delete in one transaction; `Store` exposes no
/// transaction handle, so the statements run in sequence instead.
fn delete_conversation_and_repair_team_comment_anchor(
    store: &Store,
    project_id: &str,
    conversation_id: &str,
) -> Result<(), StoreError> {
    let now = now_ms();
    let binding = store.query_one(
        "SELECT visibility, resource_state AS resourceState FROM workspace_projects WHERE project_id = ?1",
        [project_id],
        |row| {
            Ok::<_, rusqlite::Error>((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
            ))
        },
    )?;
    let is_team = binding
        .as_ref()
        .is_some_and(|(visibility, resource_state)| {
            visibility.as_deref() == Some("team") && resource_state.as_deref() != Some("deleted")
        });
    if is_team {
        ensure_team_project_comment_conversations(store, project_id, now, Some(conversation_id))?;
        if let Some(anchor_id) = get_project_comment_anchor_conversation_id(
            store,
            project_id,
            Some(conversation_id),
        )? {
            store.execute(
                "UPDATE preview_comments SET conversation_id = ?1
                  WHERE project_id = ?2 AND conversation_id = ?3",
                rusqlite::params![anchor_id, project_id, conversation_id],
            )?;
        }
    }
    store.execute(
        "DELETE FROM conversations WHERE id = ?1",
        [conversation_id],
    )?;
    Ok(())
}

fn ensure_team_project_comment_conversations(
    store: &Store,
    project_id: &str,
    now: i64,
    exclude_conversation_id: Option<&str>,
) -> Result<(), StoreError> {
    ensure_project_comment_anchor_conversation(store, project_id, now, exclude_conversation_id)?;
    ensure_project_comment_routing_conversation(store, project_id, now, exclude_conversation_id)?;
    Ok(())
}

fn ensure_project_comment_anchor_conversation(
    store: &Store,
    project_id: &str,
    now: i64,
    exclude_conversation_id: Option<&str>,
) -> Result<(), StoreError> {
    if get_project_comment_anchor_conversation_id(store, project_id, exclude_conversation_id)?
        .is_some()
    {
        return Ok(());
    }
    if store.get_project(project_id)?.is_none() {
        return Ok(());
    }
    insert_conversation(
        store,
        &format!("{PROJECT_COMMENT_ANCHOR_PREFIX}{}", random_id()),
        project_id,
        Value::Null,
        "design",
        now,
    )?;
    Ok(())
}

fn ensure_project_comment_routing_conversation(
    store: &Store,
    project_id: &str,
    now: i64,
    exclude_conversation_id: Option<&str>,
) -> Result<(), StoreError> {
    if get_latest_conversation_id_for_project(store, project_id, exclude_conversation_id)?.is_some() {
        return Ok(());
    }
    if store.get_project(project_id)?.is_none() {
        return Ok(());
    }
    insert_conversation(
        store,
        &format!("conversation-{}", random_id()),
        project_id,
        Value::Null,
        "design",
        now,
    )?;
    Ok(())
}

fn get_latest_conversation_id_for_project(
    store: &Store,
    project_id: &str,
    exclude_conversation_id: Option<&str>,
) -> Result<Option<String>, StoreError> {
    let found = store.query_one(
        "SELECT id FROM conversations
          WHERE project_id = ?1
            AND id NOT LIKE ?2
            AND (?3 IS NULL OR id != ?3)
          ORDER BY updated_at DESC, rowid DESC
          LIMIT 1",
        rusqlite::params![
            project_id,
            format!("{PROJECT_COMMENT_ANCHOR_PREFIX}%"),
            exclude_conversation_id
        ],
        |row| row.get::<_, String>(0),
    )?;
    Ok(found)
}

fn get_project_comment_anchor_conversation_id(
    store: &Store,
    project_id: &str,
    exclude_conversation_id: Option<&str>,
) -> Result<Option<String>, StoreError> {
    let found = store.query_one(
        "SELECT id FROM conversations
          WHERE project_id = ?1
            AND id LIKE ?2
            AND (?3 IS NULL OR id != ?3)
          ORDER BY created_at ASC, rowid ASC
          LIMIT 1",
        rusqlite::params![
            project_id,
            format!("{PROJECT_COMMENT_ANCHOR_PREFIX}%"),
            exclude_conversation_id
        ],
        |row| row.get::<_, String>(0),
    )?;
    Ok(found)
}

// ---- messages ----------------------------------------------------------------

fn list_messages(store: &Store, conversation_id: &str) -> Result<Vec<Value>, StoreError> {
    let rows = store.query(
        &format!(
            "SELECT {LIST_MESSAGE_COLUMNS} FROM messages
              WHERE conversation_id = ?1
              ORDER BY position ASC"
        ),
        [conversation_id],
        |row| map_message_row(row, true),
    )?;
    // One conversation-level read each for batches and artifact refs — a
    // per-message lookup would be an N+1 on every transcript.
    let batches = read_conversation_message_event_batches(store, conversation_id)?;
    let artifact_refs = conversation_chat_artifact_refs(store, conversation_id);
    let mut messages = Vec::with_capacity(rows.len());
    for row in &rows {
        let id = row.get("id").and_then(Value::as_str);
        let empty_batches: Vec<Vec<Value>> = Vec::new();
        let empty_refs: Vec<Value> = Vec::new();
        let row_batches = id
            .and_then(|id| batches.get(id))
            .map(Vec::as_slice)
            .unwrap_or(&empty_batches);
        let refs = id
            .and_then(|id| artifact_refs.get(id))
            .map(Vec::as_slice)
            .unwrap_or(&empty_refs);
        messages.push(normalize_message(row, row_batches, refs));
    }
    Ok(messages)
}

fn get_message(
    store: &Store,
    id: &str,
    conversation_id: Option<&str>,
) -> Result<Option<Value>, StoreError> {
    let row = match conversation_id {
        Some(conversation_id) => store.query_one(
            &format!(
                "SELECT {GET_MESSAGE_COLUMNS} FROM messages WHERE id = ?1 AND conversation_id = ?2"
            ),
            rusqlite::params![id, conversation_id],
            |row| map_message_row(row, false),
        )?,
        None => store.query_one(
            &format!("SELECT {GET_MESSAGE_COLUMNS} FROM messages WHERE id = ?1"),
            [id],
            |row| map_message_row(row, false),
        )?,
    };
    let Some(row) = row else {
        return Ok(None);
    };
    let message_id = row
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_default();
    let batches = read_message_event_batches(store, &message_id)?;
    let refs = message_chat_artifact_refs(store, &message_id);
    Ok(Some(normalize_message(&row, &batches, &refs)))
}

/// Parity: `listSiblingRunDoneKeys` — the first well-formed `done_key` of every
/// other assistant row, decided inside SQLite.
fn list_sibling_run_done_keys(
    store: &Store,
    conversation_id: &str,
    exclude_message_id: &str,
) -> Result<HashSet<String>, StoreError> {
    let rows = store.query(
        "SELECT (
            SELECT json_extract(event.value, '$.key')
              FROM json_each(m.events_json) AS event
             WHERE event.type = 'object'
               AND json_extract(event.value, '$.kind') = 'done_key'
               AND json_type(event.value, '$.key') = 'text'
               AND json_extract(event.value, '$.key') <> ''
             ORDER BY CAST(event.key AS INTEGER)
             LIMIT 1
          ) AS doneKey
           FROM messages AS m
          WHERE m.conversation_id = ?1
            AND m.role = 'assistant'
            AND m.id <> ?
            AND m.events_json IS NOT NULL
            AND m.events_json LIKE '%\"done_key\"%'
            AND json_valid(m.events_json)
            AND json_type(m.events_json) = 'array'",
        rusqlite::params![conversation_id, exclude_message_id],
        |row| row.get::<_, Option<String>>(0),
    )?;
    Ok(rows
        .into_iter()
        .flatten()
        .filter(|key| !key.is_empty())
        .collect())
}

/// Parity: `upsertMessage`. The response is the re-read row normalized WITHOUT
/// artifact refs (the TypeScript helper passes no `artifactRefs` argument).
fn upsert_message(
    store: &Store,
    conversation_id: &str,
    message: Value,
) -> Result<Value, StoreError> {
    let m = &message;
    let persisted_events: Option<Value> = match m.get("events") {
        Some(Value::Array(events)) => Some(Value::Array(compact_adjacent_message_agent_events(
            events.clone(),
        ))),
        other => other.cloned(),
    };
    let id_binding = sql_bind(m.get("id").unwrap_or(&Value::Null))?;
    let existing = store.query_one(
        "SELECT position, run_id AS runId, run_status AS runStatus,
                content, events_json AS eventsJson,
                task_analytics_json AS taskAnalyticsJson,
                EXISTS(
                    SELECT 1 FROM message_event_batches AS batch
                     WHERE batch.message_id = messages.id
                ) AS hasEventBatches
           FROM messages WHERE id = ?1",
        [&id_binding],
        |row| {
            let mut out = Map::new();
            for name in [
                "position",
                "runId",
                "runStatus",
                "content",
                "eventsJson",
                "taskAnalyticsJson",
                "hasEventBatches",
            ] {
                out.insert(name.to_string(), column(row, name)?);
            }
            Ok::<_, rusqlite::Error>(out)
        },
    )?;
    let now = now_ms();
    match existing {
        Some(existing) => {
            let incoming_run_is_terminal =
                is_terminal_message_run_status(m.get("runStatus").and_then(Value::as_str));
            let existing_has_batches = existing
                .get("hasEventBatches")
                .and_then(Value::as_i64)
                == Some(1);
            let existing_run_id = existing.get("runId").and_then(Value::as_str);
            let existing_run_status = existing.get("runStatus").and_then(Value::as_str);
            let preserve_daemon_event_snapshot = existing_has_batches
                || (existing_run_id.is_some()
                    && matches!(existing_run_status, Some("queued") | Some("running"))
                    && !incoming_run_is_terminal);
            let next_events_json = if preserve_daemon_event_snapshot {
                existing
                    .get("eventsJson")
                    .cloned()
                    .unwrap_or(Value::Null)
            } else if persisted_events.as_ref().is_some_and(js_truthy) {
                Value::String(serialize_run_events_for_storage(
                    persisted_events.as_ref().unwrap(),
                ))
            } else {
                Value::Null
            };
            let next_content = if preserve_daemon_event_snapshot {
                Value::String(
                    existing
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                )
            } else {
                match m.get("content") {
                    Some(value) => value.clone(),
                    None => Value::Null,
                }
            };
            let next_task_analytics_json = match m.get("taskAnalytics") {
                None => existing
                    .get("taskAnalyticsJson")
                    .cloned()
                    .unwrap_or(Value::Null),
                Some(value) if js_truthy(value) => Value::String(value.to_string()),
                Some(_) => Value::Null,
            };
            let next_content = sql_bind(&next_content)?;
            let next_events_json = sql_bind(&next_events_json)?;
            let next_task_analytics_json = sql_bind(&next_task_analytics_json)?;
            let role = sql_bind(m.get("role").unwrap_or(&Value::Null))?;
            let agent_id = sql_bind(m.get("agentId").unwrap_or(&Value::Null))?;
            let agent_name = sql_bind(m.get("agentName").unwrap_or(&Value::Null))?;
            let run_id = sql_bind(m.get("runId").unwrap_or(&Value::Null))?;
            let run_status = sql_bind(m.get("runStatus").unwrap_or(&Value::Null))?;
            let result_delivery_state = normalize_result_delivery_state_for_storage(
                m.get("resultDeliveryState"),
            );
            let last_run_event_id = sql_bind(m.get("lastRunEventId").unwrap_or(&Value::Null))?;
            let attachments = json_string_if_truthy(m, "attachments")?;
            let comment_attachments = json_string_if_truthy(m, "commentAttachments")?;
            let produced_files = json_string_if_truthy(m, "producedFiles")?;
            let trace_object_files = json_string_if_truthy(m, "traceObjectFiles")?;
            let feedback = json_string_if_truthy(m, "feedback")?;
            let pre_turn_file_names = json_string_if_truthy(m, "preTurnFileNames")?;
            let session_mode = normalize_message_session_mode_for_storage(m.get("sessionMode"));
            let run_context = json_string_if_truthy(m, "runContext")?;
            let applied_plugin_snapshot = json_string_if_truthy(m, "appliedPluginSnapshot")?;
            let forked_into = normalize_forked_into_for_storage(m.get("forkedInto"));
            let cancel_origin = normalize_cancel_origin_for_storage(m.get("cancelOrigin"));
            let telemetry_flag: i64 =
                if m.get("telemetryFinalized") == Some(&Value::Bool(true)) {
                    1
                } else {
                    0
                };
            let started_at = sql_bind(m.get("startedAt").unwrap_or(&Value::Null))?;
            let ended_at = sql_bind(m.get("endedAt").unwrap_or(&Value::Null))?;
            store.execute(
                "UPDATE messages
                    SET role = ?1, content = ?2, agent_id = ?3, agent_name = ?4,
                        run_id = ?5, run_status = ?6, result_delivery_state = ?7,
                        last_run_event_id = ?8,
                        events_json = ?9, attachments_json = ?10, comment_attachments_json = ?11,
                        produced_files_json = ?12, trace_object_files_json = ?13,
                        feedback_json = ?14,
                        pre_turn_file_names_json = ?15,
                        session_mode = ?16, run_context_json = ?17, task_analytics_json = ?18,
                        applied_plugin_snapshot_json = ?19, forked_into_json = ?20,
                        cancel_origin = ?21,
                        telemetry_finalized_at = CASE
                          WHEN ?22 THEN COALESCE(telemetry_finalized_at, ?23)
                          ELSE telemetry_finalized_at
                        END,
                        started_at = ?24, ended_at = ?25
                  WHERE id = ?26",
                rusqlite::params![
                    role,
                    next_content,
                    agent_id,
                    agent_name,
                    run_id,
                    run_status,
                    result_delivery_state,
                    last_run_event_id,
                    next_events_json,
                    attachments,
                    comment_attachments,
                    produced_files,
                    trace_object_files,
                    feedback,
                    pre_turn_file_names,
                    session_mode,
                    run_context,
                    next_task_analytics_json,
                    applied_plugin_snapshot,
                    forked_into,
                    cancel_origin,
                    telemetry_flag,
                    now,
                    started_at,
                    ended_at,
                    id_binding
                ],
            )?;
        }
        None => {
            let position = store.query_one(
                "SELECT COALESCE(MAX(position), -1) AS m FROM messages WHERE conversation_id = ?1",
                [conversation_id],
                |row| row.get::<_, i64>(0),
            )?;
            let position = position.unwrap_or(-1) + 1;
            let created_at = match m.get("createdAt").and_then(Value::as_f64) {
                Some(value) if value.is_finite() => json_from_f64(value),
                _ => json!(now),
            };
            let created_at = sql_bind(&created_at)?;
            let role = sql_bind(m.get("role").unwrap_or(&Value::Null))?;
            let content = sql_bind(m.get("content").unwrap_or(&Value::Null))?;
            let agent_id = sql_bind(m.get("agentId").unwrap_or(&Value::Null))?;
            let agent_name = sql_bind(m.get("agentName").unwrap_or(&Value::Null))?;
            let run_id = sql_bind(m.get("runId").unwrap_or(&Value::Null))?;
            let run_status = sql_bind(m.get("runStatus").unwrap_or(&Value::Null))?;
            let result_delivery_state = normalize_result_delivery_state_for_storage(
                m.get("resultDeliveryState"),
            );
            let last_run_event_id = sql_bind(m.get("lastRunEventId").unwrap_or(&Value::Null))?;
            let events_json = if persisted_events.as_ref().is_some_and(js_truthy) {
                json_text(&serialize_run_events_for_storage(
                    persisted_events.as_ref().unwrap(),
                ))
            } else {
                rusqlite::types::Value::Null
            };
            let attachments = json_string_if_truthy(m, "attachments")?;
            let comment_attachments = json_string_if_truthy(m, "commentAttachments")?;
            let produced_files = json_string_if_truthy(m, "producedFiles")?;
            let trace_object_files = json_string_if_truthy(m, "traceObjectFiles")?;
            let feedback = json_string_if_truthy(m, "feedback")?;
            let pre_turn_file_names = json_string_if_truthy(m, "preTurnFileNames")?;
            let session_mode = normalize_message_session_mode_for_storage(m.get("sessionMode"));
            let run_context = json_string_if_truthy(m, "runContext")?;
            let task_analytics = match m.get("taskAnalytics") {
                Some(value) if js_truthy(value) => json_text(&value.to_string()),
                _ => rusqlite::types::Value::Null,
            };
            let applied_plugin_snapshot = json_string_if_truthy(m, "appliedPluginSnapshot")?;
            let forked_into = normalize_forked_into_for_storage(m.get("forkedInto"));
            let cancel_origin = normalize_cancel_origin_for_storage(m.get("cancelOrigin"));
            let telemetry_finalized_at: Option<i64> =
                if m.get("telemetryFinalized") == Some(&Value::Bool(true)) {
                    Some(now)
                } else {
                    None
                };
            let started_at = sql_bind(m.get("startedAt").unwrap_or(&Value::Null))?;
            let ended_at = sql_bind(m.get("endedAt").unwrap_or(&Value::Null))?;
            store.execute(
                "INSERT INTO messages
                   (id, conversation_id, role, content, agent_id, agent_name,
                    run_id, run_status, result_delivery_state, last_run_event_id, events_json,
                    attachments_json, comment_attachments_json, produced_files_json,
                    trace_object_files_json, feedback_json, pre_turn_file_names_json,
                    session_mode, run_context_json, task_analytics_json,
                    applied_plugin_snapshot_json, forked_into_json, cancel_origin,
                    telemetry_finalized_at, started_at, ended_at, position, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                         ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28)",
                rusqlite::params![
                    id_binding,
                    conversation_id,
                    role,
                    content,
                    agent_id,
                    agent_name,
                    run_id,
                    run_status,
                    result_delivery_state,
                    last_run_event_id,
                    events_json,
                    attachments,
                    comment_attachments,
                    produced_files,
                    trace_object_files,
                    feedback,
                    pre_turn_file_names,
                    session_mode,
                    run_context,
                    task_analytics,
                    applied_plugin_snapshot,
                    forked_into,
                    cancel_origin,
                    telemetry_finalized_at,
                    started_at,
                    ended_at,
                    position,
                    created_at
                ],
            )?;
        }
    }
    if let Some(message_id) = m.get("id").and_then(Value::as_str) {
        seed_message_artifact_refs_if_absent(store, message_id, m.get("artifactRefs"));
    }
    // Bump conversation activity so the sidebar's recency sort works.
    store.execute(
        "UPDATE conversations SET updated_at = ?1 WHERE id = ?2",
        rusqlite::params![now, conversation_id],
    )?;
    let row = store.query_one(
        &format!(
            "SELECT {LIST_MESSAGE_COLUMNS} FROM messages WHERE id = ?1"
        ),
        [&id_binding],
        |row| map_message_row(row, true),
    )?;
    match row {
        Some(row) => {
            let message_id = row
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_default();
            let batches = read_message_event_batches(store, &message_id)?;
            Ok(normalize_message(&row, &batches, &[]))
        }
        None => Ok(Value::Null),
    }
}

fn map_message_row(
    row: &rusqlite::Row<'_>,
    with_task_analytics: bool,
) -> rusqlite::Result<Map<String, Value>> {
    let mut out = Map::new();
    for name in [
        "id",
        "role",
        "content",
        "agentId",
        "agentName",
        "runId",
        "runStatus",
        "resultDeliveryState",
        "lastRunEventId",
        "eventsJson",
        "attachmentsJson",
        "commentAttachmentsJson",
        "producedFilesJson",
        "traceObjectFilesJson",
        "feedbackJson",
        "preTurnFileNamesJson",
        "sessionMode",
        "runContextJson",
        "appliedPluginSnapshotJson",
        "forkedIntoJson",
        "cancelOrigin",
        "createdAt",
        "startedAt",
        "endedAt",
    ] {
        out.insert(name.to_string(), column(row, name)?);
    }
    if with_task_analytics {
        out.insert(
            "taskAnalyticsJson".to_string(),
            column(row, "taskAnalyticsJson")?,
        );
    }
    Ok(out)
}

// ---- message normalization ----------------------------------------------------

/// Parity: `normalizeMessage`. The 64 KiB-per-event payload budget
/// (`boundPersistedAgentEvents`) and the background maintenance scheduling it
/// triggers are not ported — events are returned verbatim.
fn normalize_message(
    row: &Map<String, Value>,
    batches: &[Vec<Value>],
    artifact_refs: &[Value],
) -> Value {
    let events_json = row.get("eventsJson").and_then(Value::as_str);
    let materialized = materialize_message_agent_events(events_json, batches);
    let role = row.get("role").and_then(Value::as_str);
    let visible_events = if role == Some("assistant") {
        scrub_dsml_tool_protocol_tail_from_events(&materialized.events)
    } else {
        materialized.events.clone()
    };
    let raw_content = row
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let merged_content = format!("{raw_content}{}", materialized.text_delta);
    let content = if role == Some("assistant") {
        scrub_dsml_tool_protocol_tail(&merged_content)
    } else {
        merged_content
    };

    let mut out = Map::new();
    insert_defined(&mut out, "id", row.get("id"));
    insert_defined(&mut out, "role", row.get("role"));
    out.insert("content".to_string(), json!(content));
    insert_defined(&mut out, "agentId", row.get("agentId"));
    insert_defined(&mut out, "agentName", row.get("agentName"));
    insert_defined(&mut out, "runId", row.get("runId"));
    insert_defined(&mut out, "runStatus", row.get("runStatus"));
    if let Some(state) = normalize_result_delivery_state(row.get("resultDeliveryState")) {
        out.insert("resultDeliveryState".to_string(), json!(state));
    }
    insert_defined(&mut out, "lastRunEventId", row.get("lastRunEventId"));
    if events_json.is_some() || materialized.batch_count > 0 {
        out.insert("events".to_string(), Value::Array(visible_events));
    }
    insert_parsed_json(&mut out, "attachments", row.get("attachmentsJson"));
    insert_parsed_json(
        &mut out,
        "commentAttachments",
        row.get("commentAttachmentsJson"),
    );
    insert_parsed_json(&mut out, "producedFiles", row.get("producedFilesJson"));
    if !artifact_refs.is_empty() {
        out.insert(
            "artifactRefs".to_string(),
            Value::Array(artifact_refs.to_vec()),
        );
    }
    insert_parsed_json(
        &mut out,
        "traceObjectFiles",
        row.get("traceObjectFilesJson"),
    );
    insert_parsed_json(&mut out, "feedback", row.get("feedbackJson"));
    insert_parsed_json(
        &mut out,
        "preTurnFileNames",
        row.get("preTurnFileNamesJson"),
    );
    if let Some(mode) = normalize_message_session_mode(row.get("sessionMode")) {
        out.insert("sessionMode".to_string(), json!(mode));
    }
    insert_parsed_json(&mut out, "runContext", row.get("runContextJson"));
    insert_parsed_json(&mut out, "taskAnalytics", row.get("taskAnalyticsJson"));
    insert_parsed_json(
        &mut out,
        "appliedPluginSnapshot",
        row.get("appliedPluginSnapshotJson"),
    );
    if let Some(forked) = normalize_forked_into(parse_json_or_undef(
        row.get("forkedIntoJson"),
    )) {
        out.insert("forkedInto".to_string(), forked);
    }
    if let Some(origin) = normalize_cancel_origin(row.get("cancelOrigin")) {
        out.insert("cancelOrigin".to_string(), json!(origin));
    }
    insert_defined(&mut out, "createdAt", row.get("createdAt"));
    insert_defined(&mut out, "startedAt", row.get("startedAt"));
    insert_defined(&mut out, "endedAt", row.get("endedAt"));
    Value::Object(out)
}

/// Parity: `materializeMessageAgentEvents` minus the batch maintenance hooks.
fn materialize_message_agent_events(
    events_json: Option<&str>,
    batches: &[Vec<Value>],
) -> MaterializedEvents {
    let parsed = events_json
        .filter(|value| !value.is_empty())
        .and_then(|value| serde_json::from_str::<Value>(value).ok());
    let base_events = match parsed {
        Some(Value::Array(events)) => events,
        _ => Vec::new(),
    };
    let mut events = compact_adjacent_message_agent_events(base_events);
    let mut text_delta = String::new();
    for batch in batches {
        for event in batch {
            if event.get("kind").and_then(Value::as_str) != Some("text") {
                continue;
            }
            let Some(text) = event.get("text").and_then(Value::as_str) else {
                continue;
            };
            text_delta.push_str(&strip_artifact_focus_markers(&strip_next_step_markers(
                &strip_done_markers(text),
            )));
        }
        events = merge_message_agent_events(&events, batch);
    }
    MaterializedEvents {
        events,
        batch_count: batches.len(),
        text_delta,
    }
}

struct MaterializedEvents {
    events: Vec<Value>,
    batch_count: usize,
    text_delta: String,
}

fn read_message_event_batches(
    store: &Store,
    message_id: &str,
) -> Result<Vec<Vec<Value>>, StoreError> {
    let rows = store.query(
        "SELECT events_json AS eventsJson
           FROM message_event_batches
          WHERE message_id = ?1
          ORDER BY id ASC",
        [message_id],
        |row| parse_json_column(row, "eventsJson"),
    )?;
    Ok(rows.into_iter().flatten().collect())
}

fn read_conversation_message_event_batches(
    store: &Store,
    conversation_id: &str,
) -> Result<HashMap<String, Vec<Vec<Value>>>, StoreError> {
    let rows = store.query(
        "SELECT batch.message_id AS messageId, batch.events_json AS eventsJson
           FROM message_event_batches AS batch
           JOIN messages AS message ON message.id = batch.message_id
          WHERE message.conversation_id = ?1
          ORDER BY batch.id ASC",
        [conversation_id],
        |row| {
            Ok::<_, rusqlite::Error>((
                column(row, "messageId")?,
                parse_json_column(row, "eventsJson")?,
            ))
        },
    )?;
    let mut batches: HashMap<String, Vec<Vec<Value>>> = HashMap::new();
    for (message_id, parsed) in rows {
        let Value::String(message_id) = message_id else {
            continue;
        };
        // One row is one batch; a non-array payload is skipped, never split.
        let Some(events) = parsed else {
            continue;
        };
        batches.entry(message_id).or_default().push(events);
    }
    Ok(batches)
}

/// Parity: `compactAdjacentMessageAgentEvents`.
fn compact_adjacent_message_agent_events(incoming: Vec<Value>) -> Vec<Value> {
    let mut events: Vec<Value> = Vec::new();
    let mut last_non_delta_json: Option<String> = None;
    for event in incoming {
        let kind = event
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let is_mergeable_delta = (kind == "text" || kind == "thinking")
            && event.get("text").and_then(Value::as_str).is_some();
        if is_mergeable_delta {
            let mergeable = events.last().is_some_and(|last| {
                last.get("kind").and_then(Value::as_str) == Some(kind.as_str())
                    && last.get("text").and_then(Value::as_str).is_some()
            });
            if mergeable {
                let last = events.last_mut().expect("checked above");
                let merged = format!(
                    "{}{}",
                    last.get("text").and_then(Value::as_str).unwrap_or_default(),
                    event.get("text").and_then(Value::as_str).unwrap_or_default()
                );
                if let Some(object) = last.as_object_mut() {
                    object.insert("text".to_string(), json!(merged));
                }
                last_non_delta_json = None;
                continue;
            }
            events.push(event);
            last_non_delta_json = None;
            continue;
        }
        let comparable_snapshots = events.last().is_some_and(|last| {
            last.get("kind").and_then(Value::as_str) == Some("tool_use")
                && kind == "tool_use"
                && is_todo_write_tool_name(last.get("name").and_then(Value::as_str))
                && is_todo_write_tool_name(event.get("name").and_then(Value::as_str))
                && last.get("id") == event.get("id")
                && last.get("name") == event.get("name")
        });
        if comparable_snapshots {
            let last = events.last().expect("checked above");
            if *last == event {
                continue;
            }
            let event_json = event.to_string();
            let previous_json = match &last_non_delta_json {
                Some(json) => json.clone(),
                None => last.to_string(),
            };
            if event_json == previous_json {
                last_non_delta_json = Some(previous_json);
                continue;
            }
            events.push(event);
            last_non_delta_json = Some(event_json);
            continue;
        }
        events.push(event);
        last_non_delta_json = None;
    }
    events
}

/// Parity: `mergeMessageAgentEvents`.
fn merge_message_agent_events(existing: &[Value], incoming: &[Value]) -> Vec<Value> {
    let mut events = compact_adjacent_message_agent_events(existing.to_vec());
    for event in incoming {
        if !event.is_object() {
            continue;
        }
        let kind = event
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if kind.is_empty() {
            continue;
        }
        let last_mergeable = events.last().is_some_and(|last| {
            last.get("kind").and_then(Value::as_str) == Some(kind.as_str())
                && last.get("text").and_then(Value::as_str).is_some()
        });
        let is_mergeable_delta = (kind == "text" || kind == "thinking")
            && event.get("text").and_then(Value::as_str).is_some();
        if is_mergeable_delta && last_mergeable {
            let last = events.last_mut().expect("checked above");
            let merged = format!(
                "{}{}",
                last.get("text").and_then(Value::as_str).unwrap_or_default(),
                event.get("text").and_then(Value::as_str).unwrap_or_default()
            );
            if let Some(object) = last.as_object_mut() {
                object.insert("text".to_string(), json!(merged));
            }
            continue;
        }
        if !is_mergeable_delta
            && events
                .last()
                .is_some_and(|last| *last == *event)
        {
            continue;
        }
        events.push(event.clone());
    }
    events
}

/// Parity: `isTodoWriteToolName` —
/// `^(?:todowrite|todo_write|update_plan|write_todos)$|(?:^|__)todo_?write$` (i).
fn is_todo_write_tool_name(name: Option<&str>) -> bool {
    let Some(name) = name else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "todowrite" | "todo_write" | "update_plan" | "write_todos"
    ) {
        return true;
    }
    for suffix in ["todowrite", "todo_write"] {
        if let Some(prefix) = lower.strip_suffix(suffix) {
            if prefix.is_empty() || prefix.ends_with("__") {
                return true;
            }
        }
    }
    false
}

// ---- daemon-backed merge -------------------------------------------------------

/// Parity: `mergeMessageWriteForDaemonBacked`.
///
/// `daemonKnown` is always false here: the in-memory run registry
/// (`design.runs`) belongs to the runs engine (issue .10), so
/// `daemonRunStatus()` reports "unknown" for every row and branch 1 of the
/// terminal-write arbitration ("the client is the writer") always wins.
fn merge_message_write_for_daemon_backed(
    stored: Option<&Value>,
    incoming: &Value,
    sibling_run_done_keys: &HashSet<String>,
) -> Value {
    let daemon_known = daemon_run_status(stored.and_then(|s| s.get("runId")).and_then(Value::as_str));
    let Some(stored) = stored else {
        return incoming.clone();
    };
    if stored.get("role").and_then(Value::as_str) != Some("assistant") {
        return incoming.clone();
    }
    let Some(stored_run_id) = stored
        .get("runId")
        .and_then(Value::as_str)
        .filter(|run_id| !run_id.is_empty())
    else {
        return incoming.clone();
    };
    if payload_carries_another_rows_run_stream(incoming.get("events"), sibling_run_done_keys)
        || incoming
            .get("runId")
            .and_then(Value::as_str)
            .is_some_and(|run_id| run_id != stored_run_id)
    {
        return preserve_run_fields(stored, incoming);
    }

    let incoming_events = match incoming.get("events") {
        Some(Value::Array(events)) => events.clone(),
        _ => Vec::new(),
    };
    let stored_events = stored.get("events").and_then(Value::as_array);
    let shrinks_events = stored_events
        .is_some_and(|events| !events.is_empty())
        && incoming_events.len() < stored_events.map(Vec::len).unwrap_or(0);
    let incoming_status = incoming
        .get("runStatus")
        .and_then(Value::as_str)
        .map(str::to_string);
    let stored_status = stored
        .get("runStatus")
        .and_then(Value::as_str)
        .map(str::to_string);
    let stored_status_is_terminal = stored_status
        .as_deref()
        .is_some_and(is_terminal_status);
    let regresses_terminal_status = stored_status_is_terminal
        && incoming_status.as_deref() != stored_status.as_deref();
    let incoming_is_terminal = incoming_status.as_deref().is_some_and(is_terminal_status);
    let stored_non_terminal = !stored_status_is_terminal;

    if incoming_is_terminal && stored_non_terminal && daemon_known.is_none() {
        let incoming_ended_at = incoming.get("endedAt").and_then(Value::as_f64);
        let stored_ended_at = stored.get("endedAt").and_then(Value::as_f64);
        let content = match incoming.get("content").and_then(Value::as_str) {
            Some(text) => json!(text),
            None => json!(stored
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()),
        };
        let mut out = spread_object(incoming);
        apply_run_fields(&mut out, stored);
        out.insert("runStatus".to_string(), json!(incoming_status.clone().unwrap_or_default()));
        out.insert("events".to_string(), Value::Array(incoming_events));
        out.insert("content".to_string(), content);
        set_optional(
            &mut out,
            "lastRunEventId",
            merge_last_run_event_id(
                stored.get("lastRunEventId"),
                incoming.get("lastRunEventId"),
            ),
        );
        set_optional(
            &mut out,
            "startedAt",
            stored
                .get("startedAt")
                .cloned()
                .or_else(|| incoming.get("startedAt").cloned()),
        );
        set_optional(
            &mut out,
            "endedAt",
            monotonic_ended_at(incoming_ended_at, stored_ended_at),
        );
        return Value::Object(out);
    }

    if !shrinks_events && !regresses_terminal_status {
        let stored_event_count = stored_events.map(Vec::len).unwrap_or(0);
        let events_grew = incoming_events.len() > stored_event_count;
        let incoming_text = incoming
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_string);
        let stored_text = stored
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_string);
        // TS picks `incomingTextIsStrictlyLonger` only in the `daemonKnown`
        // branch; `daemonRunStatus()` is always unknown here, so the
        // `eventsGrew` branch below is the live one.
        let merged_run_status = incoming
            .get("runStatus")
            .filter(|value| !value.is_null())
            .cloned()
            .or_else(|| stored.get("runStatus").cloned());
        let merged_content: String = if events_grew
            && incoming_text.as_ref().is_some_and(|text| !text.is_empty())
        {
            incoming_text.clone().unwrap_or_default()
        } else if stored_text.as_ref().is_some_and(|text| !text.is_empty()) {
            stored_text.clone().unwrap_or_default()
        } else {
            incoming_text.unwrap_or_default()
        };
        let mut out = spread_object(incoming);
        apply_run_fields(&mut out, stored);
        out.insert("content".to_string(), json!(merged_content));
        match merged_run_status {
            Some(value) => {
                out.insert("runStatus".to_string(), value);
            }
            None => {
                out.remove("runStatus");
            }
        }
        set_optional(
            &mut out,
            "lastRunEventId",
            merge_last_run_event_id(
                stored.get("lastRunEventId"),
                incoming.get("lastRunEventId"),
            ),
        );
        set_optional(
            &mut out,
            "startedAt",
            stored
                .get("startedAt")
                .cloned()
                .or_else(|| incoming.get("startedAt").cloned()),
        );
        set_optional(
            &mut out,
            "endedAt",
            monotonic_ended_at(
                incoming.get("endedAt").and_then(Value::as_f64),
                stored.get("endedAt").and_then(Value::as_f64),
            ),
        );
        return Value::Object(out);
    }

    let incoming_ended_at = incoming.get("endedAt").and_then(Value::as_f64);
    let stored_ended_at = stored.get("endedAt").and_then(Value::as_f64);
    let status_agrees = match (
        incoming_status.as_deref(),
        stored.get("runStatus").and_then(Value::as_str),
    ) {
        (Some(incoming), Some(stored)) => incoming == stored,
        _ => false,
    };
    let merged_content = match stored.get("content").and_then(Value::as_str) {
        Some(text) if !text.is_empty() => text.to_string(),
        _ => {
            if status_agrees {
                incoming
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            } else {
                stored
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            }
        }
    };
    let mut out = spread_object(incoming);
    apply_run_fields(&mut out, stored);
    out.insert(
        "events".to_string(),
        stored
            .get("events")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new())),
    );
    out.insert("content".to_string(), json!(merged_content));
    set_optional(&mut out, "lastRunEventId", stored.get("lastRunEventId").cloned());
    match stored.get("runStatus").cloned() {
        Some(value) => {
            out.insert("runStatus".to_string(), value);
        }
        None => {
            out.remove("runStatus");
        }
    }
    set_optional(&mut out, "startedAt", stored.get("startedAt").cloned());
    set_optional(
        &mut out,
        "endedAt",
        monotonic_ended_at(incoming_ended_at, stored_ended_at),
    );
    Value::Object(out)
}

/// The daemon-ownership fields every guarded branch restores from `stored`.
fn apply_run_fields(out: &mut Map<String, Value>, stored: &Value) {
    out.insert(
        "role".to_string(),
        stored.get("role").cloned().unwrap_or(Value::Null),
    );
    match stored.get("runId").cloned() {
        Some(value) => {
            out.insert("runId".to_string(), value);
        }
        None => {
            out.remove("runId");
        }
    }
}

fn preserve_run_fields(stored: &Value, incoming: &Value) -> Value {
    let mut out = spread_object(incoming);
    apply_run_fields(&mut out, stored);
    match stored.get("runStatus").cloned() {
        Some(value) => {
            out.insert("runStatus".to_string(), value);
        }
        None => {
            out.remove("runStatus");
        }
    }
    out.insert(
        "events".to_string(),
        stored
            .get("events")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new())),
    );
    out.insert(
        "content".to_string(),
        json!(stored
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()),
    );
    set_optional(&mut out, "producedFiles", stored.get("producedFiles").cloned());
    set_optional(
        &mut out,
        "lastRunEventId",
        stored.get("lastRunEventId").cloned(),
    );
    set_optional(&mut out, "startedAt", stored.get("startedAt").cloned());
    set_optional(&mut out, "endedAt", stored.get("endedAt").cloned());
    Value::Object(out)
}

fn monotonic_ended_at(incoming: Option<f64>, stored: Option<f64>) -> Option<Value> {
    match incoming {
        Some(incoming_value) => match stored {
            Some(stored_value) if incoming_value < stored_value => Some(json_f64(stored_value)),
            _ => Some(json_f64(incoming_value)),
        },
        None => stored.map(json_f64),
    }
}

fn set_optional(map: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    match value {
        Some(value) if !value.is_null() => {
            map.insert(key.to_string(), value);
        }
        _ => {
            map.remove(key);
        }
    }
}

/// `payloadCarriesAnotherRowsRunStream`.
fn payload_carries_another_rows_run_stream(
    incoming_events: Option<&Value>,
    sibling_run_done_keys: &HashSet<String>,
) -> bool {
    let Some(Value::Array(events)) = incoming_events else {
        return false;
    };
    if sibling_run_done_keys.is_empty() {
        return false;
    }
    events.iter().any(|event| {
        event.get("kind").and_then(Value::as_str) == Some("done_key")
            && event
                .get("key")
                .and_then(Value::as_str)
                .is_some_and(|key| sibling_run_done_keys.contains(key))
    })
}

fn parse_run_event_cursor(value: Option<&Value>) -> Option<f64> {
    let cursor = match value? {
        Value::String(_) | Value::Number(_) => js_number(value?),
        _ => return None,
    };
    if cursor.is_finite() && cursor >= 0.0 {
        Some(cursor)
    } else {
        None
    }
}

fn merge_last_run_event_id(stored: Option<&Value>, incoming: Option<&Value>) -> Option<Value> {
    let incoming_empty = incoming.is_none_or(|value| {
        value.is_null()
            || value.as_str().is_some_and(str::is_empty)
    });
    if incoming_empty {
        return stored.cloned().filter(|value| !value.is_null());
    }
    let stored_empty = stored.is_none_or(|value| {
        value.is_null()
            || value.as_str().is_some_and(str::is_empty)
    });
    if stored_empty {
        return incoming.cloned().filter(|value| !value.is_null());
    }
    match (parse_run_event_cursor(stored), parse_run_event_cursor(incoming)) {
        (Some(stored_cursor), Some(incoming_cursor)) => {
            if incoming_cursor >= stored_cursor {
                incoming.cloned()
            } else {
                stored.cloned()
            }
        }
        _ => stored.cloned().filter(|value| !value.is_null()),
    }
}

/// Parity: `design.runs.get(...)` — no runs engine yet, so no run is known.
fn daemon_run_status(_run_id: Option<&str>) -> Option<String> {
    None
}

// ---- event serialization --------------------------------------------------------

/// Parity: `serializeRunEventsForStorage`, minus the 64 KiB per-event budget.
/// A non-array payload follows the TypeScript loop: strings serialize their
/// characters, everything else yields `[]`.
fn serialize_run_events_for_storage(events: &Value) -> String {
    let parts: Vec<String> = match events {
        Value::Array(items) => items.iter().map(event_json).collect(),
        Value::String(text) => text.chars().map(|c| json!(c.to_string()).to_string()).collect(),
        _ => Vec::new(),
    };
    format!("[{}]", parts.join(","))
}

fn event_json(event: &Value) -> String {
    serde_json::to_string(event).unwrap_or_else(|_| "null".to_string())
}

// ---- strategy task turns ----------------------------------------------------------

/// Parity: `strategyTaskTurnsForRunIds`.
fn strategy_task_turns_for_run_ids(
    store: &Store,
    run_ids: &[String],
    project_id: &str,
    conversation_id: &str,
) -> Result<HashMap<String, StrategyTaskTurn>, StoreError> {
    let mut unique: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for run_id in run_ids {
        if !run_id.is_empty() && seen.insert(run_id.clone()) {
            unique.push(run_id.clone());
        }
    }
    if unique.is_empty() {
        return Ok(HashMap::new());
    }
    const CHUNK: usize = 400;
    let mut turns: HashMap<String, StrategyTaskTurn> = HashMap::new();
    for chunk in unique.chunks(CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let sql = format!(
            "SELECT r.run_id AS runId,
                    r.task_execution_id AS taskExecutionId,
                    r.task_run_index AS taskRunIndex,
                    t.outcome AS outcome,
                    t.blocked_visible_text AS blockedVisibleText
               FROM strategy_task_runs r
               LEFT JOIN strategy_task_executions t
                 ON t.task_execution_id = r.task_execution_id
              WHERE r.run_id IN ({placeholders})
                AND (t.task_execution_id IS NULL
                     OR (t.project_id = ? AND t.conversation_id = ?))"
        );
        let mut params: Vec<rusqlite::types::Value> = chunk
            .iter()
            .map(|run_id| rusqlite::types::Value::Text(run_id.clone()))
            .collect();
        params.push(rusqlite::types::Value::Text(project_id.to_string()));
        params.push(rusqlite::types::Value::Text(conversation_id.to_string()));
        let rows = store.query(&sql, rusqlite::params_from_iter(params), |row| {
            Ok::<_, rusqlite::Error>((
                column(row, "runId")?,
                column(row, "taskExecutionId")?,
                column(row, "taskRunIndex")?,
                column(row, "outcome")?,
                column(row, "blockedVisibleText")?,
            ))
        });
        let rows = match rows {
            Ok(rows) => rows,
            Err(err) if is_missing_task_store_error(&err) => return Ok(turns),
            Err(err) => return Err(err),
        };
        for (run_id, task_execution_id, task_run_index, outcome, blocked_text) in rows {
            let (Value::String(run_id), Value::String(task_execution_id)) =
                (run_id, task_execution_id)
            else {
                continue;
            };
            if !task_run_index.is_number() {
                continue;
            }
            let delivered = outcome.as_str() == Some("completed");
            let blocked = outcome.as_str() == Some("blocked");
            let blocked_text = if blocked {
                blocked_text
                    .as_str()
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_string)
            } else {
                None
            };
            turns.insert(
                run_id,
                StrategyTaskTurn {
                    task_execution_id,
                    task_run_index,
                    delivered,
                    blocked,
                    blocked_text,
                },
            );
        }
    }
    Ok(turns)
}

struct StrategyTaskTurn {
    task_execution_id: String,
    task_run_index: Value,
    delivered: bool,
    blocked: bool,
    blocked_text: Option<String>,
}

fn is_missing_task_store_error(err: &StoreError) -> bool {
    let message = err.to_string();
    let lower = message.to_ascii_lowercase();
    lower.contains("no such table: strategy_task_executions")
        || lower.contains("no such table: strategy_task_runs")
}

/// Parity: the `listMessages` route's per-message decoration.
fn decorate_message_with_strategy_turn(
    message: Value,
    turns: &HashMap<String, StrategyTaskTurn>,
) -> Value {
    let Value::Object(mut object) = message else {
        return message;
    };
    let run_id = match object.get("runId").and_then(Value::as_str) {
        Some(run_id) => run_id.to_string(),
        None => return Value::Object(object),
    };
    let Some(turn) = turns.get(&run_id) else {
        return Value::Object(object);
    };
    object.insert(
        "strategyTaskExecutionId".to_string(),
        json!(turn.task_execution_id.clone()),
    );
    object.insert(
        "strategyTaskRunIndex".to_string(),
        turn.task_run_index.clone(),
    );
    if turn.delivered {
        object.insert("strategyTaskDelivered".to_string(), json!(true));
    }
    if turn.blocked {
        object.insert("strategyTaskBlocked".to_string(), json!(true));
        object.insert(
            "strategyTaskBlockedText".to_string(),
            turn.blocked_text
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
    }
    Value::Object(object)
}

// ---- chat artifact refs ----------------------------------------------------------

const MESSAGE_ARTIFACT_COLS: &str =
    "id, message_id AS messageId, ordinal, snapshot_id AS snapshotId, \
     workspace_artifact_id AS workspaceArtifactId, \
     display_policy AS displayPolicy, label_at_capture AS labelAtCapture, kind, \
     html_version_id AS htmlVersionId, created_at AS createdAt";

fn conversation_chat_artifact_refs(
    store: &Store,
    conversation_id: &str,
) -> HashMap<String, Vec<Value>> {
    // The owning-project lookup is INSIDE the guard, matching TypeScript: a
    // failure there must never make a conversation unreadable.
    (|| -> Result<HashMap<String, Vec<Value>>, StoreError> {
        let project_id = store.query_one(
            "SELECT project_id AS projectId FROM conversations WHERE id = ?1",
            [conversation_id],
            |row| column(row, "projectId"),
        )?;
        let Some(Value::String(project_id)) = project_id else {
            return Ok(HashMap::new());
        };
        project_conversation_chat_artifact_refs(store, &project_id, conversation_id)
    })()
    .unwrap_or_default()
}

fn project_conversation_chat_artifact_refs(
    store: &Store,
    project_id: &str,
    conversation_id: &str,
) -> Result<HashMap<String, Vec<Value>>, StoreError> {
    let rows = store.query(
        &format!(
            "SELECT {MESSAGE_ARTIFACT_COLS} FROM message_artifacts
              WHERE message_id IN (SELECT id FROM messages WHERE conversation_id = ?1)
              ORDER BY message_id ASC, ordinal ASC"
        ),
        [conversation_id],
        artifact_row,
    )?;
    let mut grouped: HashMap<String, Vec<Value>> = HashMap::new();
    for row in rows {
        let message_id = row
            .get("messageId")
            .and_then(Value::as_str)
            .map(str::to_string);
        let Some(message_id) = message_id else {
            continue;
        };
        grouped
            .entry(message_id)
            .or_default()
            .push(to_ref(store, project_id, &row)?);
    }
    Ok(grouped)
}

fn message_chat_artifact_refs(store: &Store, message_id: &str) -> Vec<Value> {
    let owner = store
        .query_one(
            "SELECT c.project_id AS projectId FROM messages m
               JOIN conversations c ON c.id = m.conversation_id
              WHERE m.id = ?1",
            [message_id],
            |row| column(row, "projectId"),
        )
        .ok()
        .flatten();
    let Some(Value::String(project_id)) = owner else {
        return Vec::new();
    };
    (|| -> Result<Vec<Value>, StoreError> {
        let rows = list_message_artifact_rows(store, message_id)?;
        rows.iter().map(|row| to_ref(store, &project_id, row)).collect()
    })()
    .unwrap_or_default()
}

fn list_message_artifact_rows(
    store: &Store,
    message_id: &str,
) -> Result<Vec<Map<String, Value>>, StoreError> {
    store.query(
        &format!(
            "SELECT {MESSAGE_ARTIFACT_COLS} FROM message_artifacts
              WHERE message_id = ?1 ORDER BY ordinal ASC"
        ),
        [message_id],
        artifact_row,
    )
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

/// Parity: `toRef` in `chat-artifacts/refs.ts`.
fn to_ref(store: &Store, project_id: &str, row: &Map<String, Value>) -> Result<Value, StoreError> {
    let snapshot_id = row
        .get("snapshotId")
        .and_then(Value::as_str)
        .map(str::to_string);
    let snapshot = match &snapshot_id {
        Some(snapshot_id) => get_chat_artifact_snapshot(store, snapshot_id)?,
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
        .is_some_and(|value| value.get("captureState").and_then(Value::as_str) == Some("ready"));
    if !ready {
        return Ok(Value::Object(object));
    }
    let snapshot = snapshot.expect("checked above");
    object.insert(
        "snapshotId".to_string(),
        snapshot.get("id").cloned().unwrap_or(Value::Null),
    );
    if snapshot.get("contentDigest").is_some_and(|value| !value.is_null()) {
        object.insert(
            "snapshotUrl".to_string(),
            json!(chat_artifact_snapshot_content_url(
                project_id,
                snapshot.get("id").and_then(Value::as_str).unwrap_or_default(),
            )),
        );
    }
    if snapshot.get("thumbnailDigest").is_some_and(|value| !value.is_null()) {
        object.insert(
            "thumbnailUrl".to_string(),
            json!(chat_artifact_snapshot_thumbnail_url(
                project_id,
                snapshot.get("id").and_then(Value::as_str).unwrap_or_default(),
            )),
        );
    }
    Ok(Value::Object(object))
}

fn get_chat_artifact_snapshot(
    store: &Store,
    id: &str,
) -> Result<Option<Map<String, Value>>, StoreError> {
    store.query_one(
        "SELECT id, capture_state AS captureState, content_digest AS contentDigest, \
                thumbnail_digest AS thumbnailDigest
           FROM chat_artifact_snapshots WHERE id = ?1",
        [id],
        |row| {
            let mut out = Map::new();
            for name in ["id", "captureState", "contentDigest", "thumbnailDigest"] {
                out.insert(name.to_string(), column(row, name)?);
            }
            Ok::<_, rusqlite::Error>(out)
        },
    )
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
    const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.!~*'()";
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

fn ref_state(snapshot: Option<&Map<String, Value>>) -> &'static str {
    let Some(snapshot) = snapshot else {
        return "legacy_unavailable";
    };
    match snapshot.get("captureState").and_then(Value::as_str) {
        Some("ready") => "ready",
        Some("pending") => "pending",
        _ => "failed",
    }
}

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

/// Parity: `seedMessageArtifactRefsIfAbsent` — every failure is swallowed so a
/// fork that cannot carry its refs still forks.
fn seed_message_artifact_refs_if_absent(
    store: &Store,
    message_id: &str,
    refs: Option<&Value>,
) {
    let Some(Value::Array(refs)) = refs else {
        return;
    };
    if refs.is_empty() {
        return;
    }
    let result: Result<(), StoreError> = (|| {
        if !list_message_artifact_rows(store, message_id)?.is_empty() {
            return Ok(());
        }
        let mut inputs: Vec<Map<String, Value>> = Vec::new();
        for raw in refs {
            let label = raw.get("label").and_then(Value::as_str);
            let kind = raw.get("kind").and_then(Value::as_str);
            let display_policy = raw.get("displayPolicy").and_then(Value::as_str);
            let valid = label.is_some()
                && kind.is_some()
                && matches!(
                    display_policy,
                    Some("immutable_snapshot") | Some("latest_with_static_preview")
                );
            if !valid {
                continue;
            }
            let mut input = Map::new();
            input.insert("label".to_string(), json!(label.expect("checked above")));
            input.insert("kind".to_string(), json!(kind.expect("checked above")));
            input.insert(
                "displayPolicy".to_string(),
                json!(display_policy.expect("checked above")),
            );
            input.insert(
                "snapshotId".to_string(),
                raw.get("snapshotId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            );
            input.insert(
                "workspaceArtifactId".to_string(),
                raw.get("workspaceArtifactId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            );
            inputs.push(input);
        }
        if inputs.is_empty() {
            return Ok(());
        }
        replace_message_artifacts(store, message_id, &inputs)
    })();
    if let Err(err) = result {
        tracing::warn!(error = %err, "failed to seed message artifact refs");
    }
}

/// Parity: `replaceMessageArtifacts`. TypeScript wraps the delete and inserts
/// in one transaction; `Store` exposes no transaction handle.
fn replace_message_artifacts(
    store: &Store,
    message_id: &str,
    refs: &[Map<String, Value>],
) -> Result<(), StoreError> {
    store.execute(
        "DELETE FROM message_artifacts WHERE message_id = ?1",
        [message_id],
    )?;
    let now = now_ms();
    for (ordinal, input) in refs.iter().enumerate() {
        let snapshot_id = input
            .get("snapshotId")
            .and_then(Value::as_str)
            .map(str::to_string);
        let workspace_artifact_id = input
            .get("workspaceArtifactId")
            .and_then(Value::as_str)
            .map(str::to_string);
        store.execute(
            "INSERT INTO message_artifacts
               (message_id, ordinal, id, snapshot_id, workspace_artifact_id,
                display_policy, label_at_capture, kind,
                html_version_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9)",
            rusqlite::params![
                message_id,
                ordinal as i64,
                random_id(),
                snapshot_id,
                workspace_artifact_id,
                input
                    .get("displayPolicy")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                input.get("label").and_then(Value::as_str).unwrap_or_default(),
                input.get("kind").and_then(Value::as_str).unwrap_or_default(),
                now
            ],
        )?;
    }
    Ok(())
}

// ---- conversation helpers ---------------------------------------------------------

fn conversation_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Map<String, Value>> {
    let mut out = Map::new();
    for name in CONVERSATION_COLUMNS {
        out.insert(name.to_string(), column(row, name)?);
    }
    Ok(out)
}

fn normalize_conversation(row: &Map<String, Value>) -> Map<String, Value> {
    let mut summary_row = Map::new();
    summary_row.insert(
        "runStatus".to_string(),
        row.get("latestRunStatus").cloned().unwrap_or(Value::Null),
    );
    summary_row.insert(
        "startedAt".to_string(),
        row.get("latestRunStartedAt").cloned().unwrap_or(Value::Null),
    );
    summary_row.insert(
        "endedAt".to_string(),
        row.get("latestRunEndedAt").cloned().unwrap_or(Value::Null),
    );
    summary_row.insert(
        "eventsJson".to_string(),
        row.get("latestRunEventsJson").cloned().unwrap_or(Value::Null),
    );
    let latest_run = conversation_run_summary_from_row(&summary_row);

    let mut out = Map::new();
    out.insert("id".to_string(), row.get("id").cloned().unwrap_or(Value::Null));
    out.insert(
        "projectId".to_string(),
        row.get("projectId").cloned().unwrap_or(Value::Null),
    );
    out.insert(
        "title".to_string(),
        row.get("title").cloned().unwrap_or(Value::Null),
    );
    out.insert(
        "sessionMode".to_string(),
        json!(normalize_conversation_session_mode(
            row.get("sessionMode")
        )),
    );
    let message_count = match row.get("messageCount") {
        Some(Value::Null) | None => json!(0),
        Some(value) => json_from_f64(js_number(value)),
    };
    out.insert("messageCount".to_string(), message_count);
    out.insert(
        "createdAt".to_string(),
        json_from_f64(js_number(row.get("createdAt").unwrap_or(&Value::Null))),
    );
    out.insert(
        "updatedAt".to_string(),
        json_from_f64(js_number(row.get("updatedAt").unwrap_or(&Value::Null))),
    );
    if let Some(total) = number_property(row.get("totalDurationMs")) {
        out.insert("totalDurationMs".to_string(), total);
    }
    match latest_run {
        Some(summary) => {
            out.insert("latestRun".to_string(), summary);
        }
        None => {
            out.remove("latestRun");
        }
    }
    out
}

fn number_property(value: Option<&Value>) -> Option<Value> {
    let value = match value {
        Some(Value::Null) | None => return None,
        Some(value) => value,
    };
    let number = js_number(value);
    if number.is_finite() {
        Some(json_from_f64(number))
    } else {
        None
    }
}

fn conversation_run_summary_from_row(row: &Map<String, Value>) -> Option<Value> {
    let status = row.get("runStatus").and_then(Value::as_str)?;
    let started_at = optional_finite_number(row.get("startedAt"));
    let ended_at = optional_finite_number(row.get("endedAt"));
    let usage_duration_ms = latest_usage_duration_ms(row.get("eventsJson"));
    let duration_ms = match (started_at, ended_at) {
        (Some(started), Some(ended)) => Some((ended - started).max(0.0)),
        _ => usage_duration_ms,
    };
    let mut out = Map::new();
    out.insert("status".to_string(), json!(status));
    if let Some(started) = started_at {
        out.insert("startedAt".to_string(), json_from_f64(started));
    }
    if let Some(ended) = ended_at {
        out.insert("endedAt".to_string(), json_from_f64(ended));
    }
    if let Some(duration) = duration_ms.filter(|value| value.is_finite()) {
        out.insert("durationMs".to_string(), json_from_f64(duration));
    }
    Some(Value::Object(out))
}

/// `row.startedAt == null ? undefined : Number(row.startedAt)`, then finiteness.
fn optional_finite_number(value: Option<&Value>) -> Option<f64> {
    match value {
        Some(Value::Null) | None => None,
        Some(value) => {
            let number = js_number(value);
            number.is_finite().then_some(number)
        }
    }
}

fn latest_usage_duration_ms(events_json: Option<&Value>) -> Option<f64> {
    let Some(Value::String(events_json)) = events_json else {
        return None;
    };
    if events_json.is_empty() {
        return None;
    }
    let Ok(Value::Array(events)) = serde_json::from_str::<Value>(events_json) else {
        return None;
    };
    for event in events.iter().rev() {
        if event.get("kind").and_then(Value::as_str) != Some("usage") {
            continue;
        }
        let Some(duration) = event.get("durationMs").and_then(Value::as_f64) else {
            continue;
        };
        if duration.is_finite() {
            return Some(duration.max(0.0));
        }
    }
    None
}

/// Parity: `terminalRunDurationSql`.
fn terminal_run_duration_sql(alias: &str) -> String {
    let prefix = if alias.is_empty() {
        String::new()
    } else {
        format!("{alias}.")
    };
    format!(
        "CASE
            WHEN {prefix}started_at IS NOT NULL AND {prefix}ended_at IS NOT NULL THEN
              CASE
                WHEN CAST({prefix}ended_at AS INTEGER) >= CAST({prefix}started_at AS INTEGER)
                  THEN CAST({prefix}ended_at AS INTEGER) - CAST({prefix}started_at AS INTEGER)
                ELSE 0
              END
            ELSE (
              SELECT CASE
                       WHEN json_extract(usage_event.value, '$.durationMs') >= 0
                         THEN json_extract(usage_event.value, '$.durationMs')
                       ELSE 0
                     END
                FROM json_each(
                  CASE
                    WHEN json_valid({prefix}events_json) AND json_type({prefix}events_json) = 'array'
                      THEN {prefix}events_json
                    ELSE '[]'
                  END
                ) AS usage_event
               WHERE usage_event.type = 'object'
                 AND json_extract(usage_event.value, '$.kind') = 'usage'
                 AND json_type(usage_event.value, '$.durationMs') IN ('integer', 'real')
               ORDER BY CAST(usage_event.key AS INTEGER) DESC
               LIMIT 1
            )
          END"
    )
}

fn normalize_conversation_session_mode(value: Option<&Value>) -> &'static str {
    match value.and_then(Value::as_str) {
        Some("chat") => "chat",
        Some("plan") => "plan",
        _ => "design",
    }
}

fn is_chat_session_mode(value: Option<&Value>) -> bool {
    matches!(
        value.and_then(Value::as_str),
        Some("chat") | Some("design") | Some("plan")
    )
}

/// Parity: `updateProject(db, projectId, {})` — re-stamps `updated_at` and
/// rewrites every column from its normalized projection.
fn bump_project_updated_at(store: &Store, project_id: &str) -> Result<(), StoreError> {
    let Some(existing) = store.get_project(project_id)? else {
        return Ok(());
    };
    let metadata_json = match existing.metadata_json.as_deref() {
        Some(text) if !text.is_empty() => match serde_json::from_str::<Value>(text) {
            Ok(parsed) if js_truthy(&parsed) => Some(parsed.to_string()),
            _ => None,
        },
        _ => None,
    };
    store.execute(
        "UPDATE projects
            SET name = ?1,
                skill_id = ?2,
                design_system_id = ?3,
                pending_prompt = ?4,
                metadata_json = ?5,
                custom_instructions = ?6,
                updated_at = ?7
          WHERE id = ?8",
        rusqlite::params![
            existing.name,
            existing.skill_id,
            existing.design_system_id,
            existing.pending_prompt,
            metadata_json,
            existing.custom_instructions,
            now_ms(),
            project_id
        ],
    )?;
    Ok(())
}

// ---- fork titles -------------------------------------------------------------------

/// Parity: `nextForkedConversationTitle`.
fn next_forked_conversation_title(
    source_title: Option<&str>,
    existing_titles: &[Option<String>],
) -> Option<String> {
    let source = source_title.unwrap_or_default().trim();
    if source.is_empty() {
        return None;
    }
    let base = split_our_numbering(source)
        .map(|(base, _)| base)
        .unwrap_or_else(|| source.to_string());
    let mut highest: u32 = 0;
    for raw in existing_titles {
        let title = raw.as_deref().unwrap_or_default().trim().to_string();
        if title.is_empty() {
            continue;
        }
        let Some((existing_base, n)) = split_our_numbering(&title) else {
            continue;
        };
        if existing_base != base {
            continue;
        }
        if n > highest {
            highest = n;
        }
    }
    Some(format!("{base} ({})", highest + 1))
}

/// `OUR_NUMBER_SUFFIX = / \((\d{1,3})\)$/` plus its leading-zero / lower-bound
/// / trailing-whitespace guards.
fn split_our_numbering(title: &str) -> Option<(String, u32)> {
    if !title.ends_with(')') {
        return None;
    }
    let head = &title[..title.len() - 1];
    let open = head.rfind(" (")?;
    let digits = &title[open + 2..title.len() - 1];
    if digits.is_empty() || digits.len() > 3 {
        return None;
    }
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if digits.len() > 1 && digits.starts_with('0') {
        return None;
    }
    let n: u32 = digits.parse().ok()?;
    if n < 1 {
        return None;
    }
    let base = &title[..open];
    if base != base.trim_end() {
        return None;
    }
    Some((base.to_string(), n))
}

// ---- strip markers --------------------------------------------------------------------

const DSML_PROTOCOL_TAIL_EVENT_LOOKBACK: usize = 512;

/// Parity: `scrubDsmlToolProtocolTailFromEvents` — operates in UTF-16 units so
/// `slice`/`length` agree with JavaScript.
fn scrub_dsml_tool_protocol_tail_from_events(events: &[Value]) -> Vec<Value> {
    let mut suffix: Vec<u16> = Vec::new();
    let mut index = events.len();
    while index > 0 {
        index -= 1;
        let event = &events[index];
        if event.get("kind").and_then(Value::as_str) != Some("text") {
            continue;
        }
        let Some(text) = event.get("text").and_then(Value::as_str) else {
            continue;
        };
        let remaining = DSML_PROTOCOL_TAIL_EVENT_LOOKBACK.saturating_sub(suffix.len());
        if remaining == 0 {
            break;
        }
        let units: Vec<u16> = text.encode_utf16().collect();
        let take = remaining.min(units.len());
        let mut next = units[units.len() - take..].to_vec();
        next.extend_from_slice(&suffix);
        suffix = next;
    }

    let visible = scrub_dsml_tool_protocol_tail(&String::from_utf16_lossy(&suffix));
    let mut chars_to_remove = suffix.len().saturating_sub(visible.encode_utf16().count());
    if chars_to_remove == 0 {
        return events.to_vec();
    }

    let mut visible_events = events.to_vec();
    let mut emptied: HashSet<usize> = HashSet::new();
    let mut index = visible_events.len();
    while index > 0 && chars_to_remove > 0 {
        index -= 1;
        let event = &visible_events[index];
        if event.get("kind").and_then(Value::as_str) != Some("text") {
            continue;
        }
        let Some(text) = event.get("text").and_then(Value::as_str) else {
            continue;
        };
        let units: Vec<u16> = text.encode_utf16().collect();
        if chars_to_remove >= units.len() {
            chars_to_remove -= units.len();
            emptied.insert(index);
            continue;
        }
        let keep = units.len() - chars_to_remove;
        let next_text = String::from_utf16_lossy(&units[..keep]);
        if let Some(object) = visible_events[index].as_object_mut() {
            object.insert("text".to_string(), json!(next_text));
        }
        chars_to_remove = 0;
    }
    visible_events
        .into_iter()
        .enumerate()
        .filter_map(|(index, event)| (!emptied.contains(&index)).then_some(event))
        .collect()
}

/// Parity: `scrubDsmlToolProtocolTail` — the ordered three-tag DSML closing
/// sequence, only when it is the final suffix.
fn scrub_dsml_tool_protocol_tail(text: &str) -> String {
    match dsml_protocol_tail_start(text) {
        Some(start) => text[..start].to_string(),
        None => text.to_string(),
    }
}

fn dsml_protocol_tail_start(text: &str) -> Option<usize> {
    for start in 0..text.len() {
        if text.as_bytes().get(start) != Some(&b'<') {
            continue;
        }
        if match_dsml_protocol_tail(text, start) == Some(text.len()) {
            return Some(start);
        }
    }
    None
}

fn match_dsml_protocol_tail(text: &str, start: usize) -> Option<usize> {
    let mut index = skip_literal(text, start, "<")?;
    index = skip_literal(text, index, "/")?;
    index = skip_js_ws(text, index);
    index = skip_pipe_pair(text, index)?;
    index = skip_ci(text, index, "dsml")?;
    index = skip_js_ws(text, index);
    index = skip_pipe_pair(text, index)?;
    index = skip_ci(text, index, "parameter")?;
    index = skip_js_ws(text, index);
    index = skip_literal(text, index, ">")?;

    index = skip_js_ws(text, index);
    index = skip_literal(text, index, "<")?;
    index = skip_literal(text, index, "/")?;
    index = skip_js_ws(text, index);
    index = skip_pipe_pair(text, index)?;
    index = skip_ci(text, index, "dsml")?;
    index = skip_js_ws(text, index);
    index = skip_pipe_pair(text, index)?;
    index = skip_ci(text, index, "invoke")?;
    index = skip_js_ws(text, index);
    index = skip_literal(text, index, ">")?;

    index = skip_js_ws(text, index);
    index = skip_literal(text, index, "<")?;
    index = skip_literal(text, index, "/")?;
    index = skip_js_ws(text, index);
    index = skip_pipe_pair(text, index)?;
    index = skip_ci(text, index, "dsml")?;
    index = skip_js_ws(text, index);
    index = skip_pipe_pair(text, index)?;
    index = skip_ci(text, index, "tool_calls")?;
    index = skip_js_ws(text, index);
    index = skip_literal(text, index, ">")?;

    Some(skip_js_ws(text, index))
}

fn skip_literal(text: &str, index: usize, literal: &str) -> Option<usize> {
    let rest = text.get(index..)?;
    if rest.len() < literal.len() || !rest.as_bytes().starts_with(literal.as_bytes()) {
        return None;
    }
    Some(index + literal.len())
}

fn skip_ci(text: &str, index: usize, literal: &str) -> Option<usize> {
    let rest = text.get(index..)?;
    if rest.len() < literal.len() {
        return None;
    }
    if !rest.as_bytes()[..literal.len()].eq_ignore_ascii_case(literal.as_bytes()) {
        return None;
    }
    Some(index + literal.len())
}

/// JavaScript's `\s`.
fn is_js_ws(c: char) -> bool {
    matches!(
        c,
        '\t'
            | '\n'
            | '\u{000B}'
            | '\u{000C}'
            | '\r'
            | ' '
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

fn skip_js_ws(text: &str, mut index: usize) -> usize {
    while let Some(c) = text.get(index..).and_then(|rest| rest.chars().next()) {
        if is_js_ws(c) {
            index += c.len_utf8();
        } else {
            break;
        }
    }
    index
}

/// `(?:[|｜]\s*){2}`
fn skip_pipe_pair(text: &str, mut index: usize) -> Option<usize> {
    for _ in 0..2 {
        let c = text.get(index..)?.chars().next()?;
        if c != '|' && c != '｜' {
            return None;
        }
        index += c.len_utf8();
        index = skip_js_ws(text, index);
    }
    Some(index)
}

/// Parity: `stripDoneMarkers` — `/<od-done\b[^>]*>/gi`.
fn strip_done_markers(text: &str) -> String {
    strip_marker_tags(text, "od-done", false)
}

/// Parity: `stripArtifactFocusMarkers` — `/<\/?od-focus\b[^>]*>/gi`.
fn strip_artifact_focus_markers(text: &str) -> String {
    strip_marker_tags(text, "od-focus", true)
}

fn strip_marker_tags(text: &str, name: &str, allow_slash: bool) -> String {
    if !text.contains('<') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        if text.as_bytes().get(index) == Some(&b'<') {
            if let Some(next) = match_marker_tag(text, index, name, allow_slash) {
                index = next;
                continue;
            }
        }
        let c = text[index..].chars().next().expect("in bounds");
        out.push(c);
        index += c.len_utf8();
    }
    out
}

/// `<name\b[^>]*>` starting at `start`, returning the index after the `>`.
fn match_marker_tag(text: &str, start: usize, name: &str, allow_slash: bool) -> Option<usize> {
    let mut index = skip_literal(text, start, "<")?;
    if allow_slash {
        if let Some(next) = skip_literal(text, index, "/") {
            index = next;
        }
    }
    index = skip_ci(text, index, name)?;
    // `\b` after the name: the previous character is a word character, so the
    // next one must not be (or the match must be at end of input).
    match text.get(index..).and_then(|rest| rest.chars().next()) {
        None => {}
        Some(c) if !is_word_char(c) => {}
        Some(_) => return None,
    }
    let rest = text.get(index..)?;
    let relative = rest.find('>')?;
    Some(index + relative + 1)
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Parity: `stripNextStepMarkers` — the balanced/orphan `<od-next …>` block
/// regex followed by the trailing-line-whitespace trim.
fn strip_next_step_markers(text: &str) -> String {
    if !text.contains('<') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        if text.as_bytes().get(index) == Some(&b'<') {
            if let Some(next) = match_od_next_block(text, index) {
                index = next;
                continue;
            }
        }
        let c = text[index..].chars().next().expect("in bounds");
        out.push(c);
        index += c.len_utf8();
    }
    strip_trailing_line_ws(&out).to_string()
}

/// `/<od-next\b[^>]*>[\s\S]*?<\/od-next\s*>|<\/?od-next\b[^>]*>/gi`
fn match_od_next_block(text: &str, start: usize) -> Option<usize> {
    if let Some(open_end) = match_marker_tag(text, start, "od-next", false) {
        if let Some(close_end) = find_od_next_close(text, open_end) {
            return Some(close_end);
        }
    }
    match_marker_tag(text, start, "od-next", true)
}

fn find_od_next_close(text: &str, from: usize) -> Option<usize> {
    for index in from..text.len() {
        if text.as_bytes().get(index) != Some(&b'<') {
            continue;
        }
        if let Some(end) = match_literal_ci_tag(text, index, "od-next") {
            return Some(end);
        }
    }
    None
}

/// `<\/od-next\s*>` (no word boundary on the closing tag).
fn match_literal_ci_tag(text: &str, start: usize, name: &str) -> Option<usize> {
    let mut index = skip_literal(text, start, "<")?;
    index = skip_literal(text, index, "/")?;
    index = skip_ci(text, index, name)?;
    index = skip_js_ws(text, index);
    skip_literal(text, index, ">")
}

/// `/[ \t]*(?:\r?\n[ \t]*)+$/` removed from the end; returns the remainder.
fn strip_trailing_line_ws(text: &str) -> &str {
    let bytes = text.as_bytes();
    let mut end = text.len();
    let mut matched = false;
    loop {
        let mut index = end;
        while index > 0 && (bytes[index - 1] == b' ' || bytes[index - 1] == b'\t') {
            index -= 1;
        }
        if index == 0 || bytes[index - 1] != b'\n' {
            break;
        }
        let newline_start = if index >= 2 && bytes[index - 2] == b'\r' {
            index - 2
        } else {
            index - 1
        };
        end = newline_start;
        matched = true;
    }
    if !matched {
        return text;
    }
    let mut start = end;
    while start > 0 && (bytes[start - 1] == b' ' || bytes[start - 1] == b'\t') {
        start -= 1;
    }
    &text[..start]
}

// ---- normalize helpers ------------------------------------------------------------

fn normalize_result_delivery_state(value: Option<&Value>) -> Option<&'static str> {
    match value.and_then(Value::as_str) {
        Some("delivered") => Some("delivered"),
        Some("no_result") => Some("no_result"),
        Some("delivery_failed") => Some("delivery_failed"),
        _ => None,
    }
}

fn normalize_result_delivery_state_for_storage(value: Option<&Value>) -> rusqlite::types::Value {
    match normalize_result_delivery_state(value) {
        Some(state) => rusqlite::types::Value::Text(state.to_string()),
        None => rusqlite::types::Value::Null,
    }
}

fn normalize_message_session_mode(value: Option<&Value>) -> Option<&'static str> {
    match value.and_then(Value::as_str) {
        Some("chat") => Some("chat"),
        Some("design") => Some("design"),
        Some("plan") => Some("plan"),
        _ => None,
    }
}

fn normalize_message_session_mode_for_storage(value: Option<&Value>) -> rusqlite::types::Value {
    match normalize_message_session_mode(value) {
        Some(mode) => rusqlite::types::Value::Text(mode.to_string()),
        None => rusqlite::types::Value::Null,
    }
}

fn normalize_forked_into(value: Option<Value>) -> Option<Value> {
    let value = value?;
    if !value.is_object() {
        return None;
    }
    let title = value.get("title").and_then(Value::as_str)?;
    if title.is_empty() {
        return None;
    }
    let mut out = Map::new();
    out.insert("title".to_string(), json!(title));
    if let Some(conversation_id) = value
        .get("conversationId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        out.insert("conversationId".to_string(), json!(conversation_id));
    }
    Some(Value::Object(out))
}

fn normalize_forked_into_for_storage(value: Option<&Value>) -> rusqlite::types::Value {
    match normalize_forked_into(value.cloned()) {
        Some(value) => rusqlite::types::Value::Text(value.to_string()),
        None => rusqlite::types::Value::Null,
    }
}

fn normalize_cancel_origin(value: Option<&Value>) -> Option<&'static str> {
    match value.and_then(Value::as_str) {
        Some("user_stop") => Some("user_stop"),
        Some("project_cleanup") => Some("project_cleanup"),
        Some("daemon_shutdown") => Some("daemon_shutdown"),
        Some("unknown") => Some("unknown"),
        _ => None,
    }
}

fn normalize_cancel_origin_for_storage(value: Option<&Value>) -> rusqlite::types::Value {
    match normalize_cancel_origin(value) {
        Some(origin) => rusqlite::types::Value::Text(origin.to_string()),
        None => rusqlite::types::Value::Null,
    }
}

fn is_terminal_status(status: &str) -> bool {
    TERMINAL_RUN_STATUSES.contains(&status)
}

fn is_terminal_message_run_status(status: Option<&str>) -> bool {
    status.is_some_and(is_terminal_status)
}

// ---- row / value helpers -------------------------------------------------------------

fn column(row: &rusqlite::Row<'_>, name: &str) -> rusqlite::Result<Value> {
    match row.get::<_, rusqlite::types::Value>(name) {
        Ok(raw) => Ok(sql_value_to_json(raw)),
        // A projection that never selected the column reads as `undefined`,
        // exactly like better-sqlite3's `row[name]`.
        Err(rusqlite::Error::InvalidColumnName(_)) => Ok(Value::Null),
        Err(err) => Err(err),
    }
}

fn optional_column(row: &rusqlite::Row<'_>, name: &str) -> rusqlite::Result<Value> {
    column(row, name)
}

fn sql_value_to_json(raw: rusqlite::types::Value) -> Value {
    match raw {
        rusqlite::types::Value::Null => Value::Null,
        rusqlite::types::Value::Integer(value) => json!(value),
        rusqlite::types::Value::Real(value) => json_f64(value),
        rusqlite::types::Value::Text(value) => json!(value),
        rusqlite::types::Value::Blob(value) => {
            json!({ "type": "Buffer", "data": value })
        }
    }
}

/// `parseJsonOrUndef` + the `Array.isArray` guard the batch readers apply:
/// non-array (and non-text) payloads yield `None`, never an empty batch.
fn parse_json_column(
    row: &rusqlite::Row<'_>,
    name: &str,
) -> rusqlite::Result<Option<Vec<Value>>> {
    let raw = column(row, name)?;
    Ok(match raw {
        Value::String(text) if !text.is_empty() => serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|value| match value {
                Value::Array(events) => Some(events),
                _ => None,
            }),
        _ => None,
    })
}

/// `undefined`/`null` omit the key; anything else is stored.
fn insert_defined(map: &mut Map<String, Value>, key: &str, value: Option<&Value>) {
    match value {
        Some(Value::Null) | None => {}
        Some(value) => {
            map.insert(key.to_string(), value.clone());
        }
    }
}

/// `parseJsonOrUndef` + insertion: only a parsed value lands, and only when the
/// column held a non-empty JSON string.
fn insert_parsed_json(map: &mut Map<String, Value>, key: &str, value: Option<&Value>) {
    if let Some(parsed) = parse_json_or_undef(value) {
        map.insert(key.to_string(), parsed);
    }
}

fn parse_json_or_undef(value: Option<&Value>) -> Option<Value> {
    let Value::String(text) = value? else {
        return None;
    };
    if text.is_empty() {
        return None;
    }
    serde_json::from_str::<Value>(text).ok()
}

fn json_text(text: &str) -> rusqlite::types::Value {
    rusqlite::types::Value::Text(text.to_string())
}

/// `sql_bind`: a JSON value the way better-sqlite3 would bind it. Booleans,
/// arrays and objects are rejected — the TypeScript daemon cannot bind them
/// either, so both sides fail the statement.
fn sql_bind(value: &Value) -> Result<rusqlite::types::Value, StoreError> {
    match value {
        Value::Null => Ok(rusqlite::types::Value::Null),
        Value::Number(number) => Ok(match number.as_i64() {
            Some(integer) => rusqlite::types::Value::Integer(integer),
            None => match number.as_f64() {
                Some(real) if real.is_finite() => rusqlite::types::Value::Real(real),
                _ => rusqlite::types::Value::Null,
            },
        }),
        Value::String(text) => Ok(rusqlite::types::Value::Text(text.clone())),
        _ => Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
            std::fmt::Error,
        ))
        .into()),
    }
}

/// `m.attachments ? JSON.stringify(m.attachments) : null`
fn json_string_if_truthy(m: &Value, key: &str) -> Result<rusqlite::types::Value, StoreError> {
    match m.get(key) {
        Some(value) if js_truthy(value) => Ok(json_text(&value.to_string())),
        _ => Ok(rusqlite::types::Value::Null),
    }
}

fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number
            .as_f64()
            .is_some_and(|number| number != 0.0 && !number.is_nan()),
        Value::String(text) => !text.is_empty(),
        _ => true,
    }
}

/// JavaScript's `Number()` coercion.
fn js_number(value: &Value) -> f64 {
    match value {
        Value::Null => 0.0,
        Value::Bool(flag) => {
            if *flag {
                1.0
            } else {
                0.0
            }
        }
        Value::Number(number) => number.as_f64().unwrap_or(f64::NAN),
        Value::String(text) => js_number_from_str(text),
        _ => f64::NAN,
    }
}

fn js_number_from_str(text: &str) -> f64 {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return 0.0;
    }
    match trimmed {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let (sign, digits) = match trimmed.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => match trimmed.strip_prefix('+') {
            Some(rest) => (1.0, rest),
            None => (1.0, trimmed),
        },
    };
    for (prefix, radix) in [("0x", 16u32), ("0X", 16), ("0b", 2), ("0B", 2), ("0o", 8), ("0O", 8)]
    {
        if let Some(rest) = digits.strip_prefix(prefix) {
            if rest.is_empty() || !rest.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
                return f64::NAN;
            }
            let mut value: f64 = 0.0;
            for byte in rest.bytes() {
                let digit = match byte {
                    b'0'..=b'9' => u32::from(byte - b'0'),
                    b'a'..=b'f' => u32::from(byte - b'a') + 10,
                    b'A'..=b'F' => u32::from(byte - b'A') + 10,
                    _ => return f64::NAN,
                };
                if digit >= radix {
                    return f64::NAN;
                }
                value = value * f64::from(radix) + f64::from(digit);
            }
            return sign * value;
        }
    }
    if !trimmed
        .bytes()
        .all(|byte| matches!(byte, b'0'..=b'9' | b'+' | b'-' | b'.' | b'e' | b'E'))
    {
        return f64::NAN;
    }
    trimmed.parse::<f64>().unwrap_or(f64::NAN)
}

fn number_as_f64_finite(value: &Value) -> bool {
    js_number(value).is_finite()
}

/// A finite whole number serializes like JavaScript's `Number` (no `.0`).
fn json_f64(value: f64) -> Value {
    if value.is_finite()
        && value.fract() == 0.0
        && value.abs() <= 9_007_199_254_740_992.0
    {
        json!(value as i64)
    } else if value.is_finite() {
        json!(value)
    } else {
        Value::Null
    }
}

fn json_from_f64(value: f64) -> Value {
    json_f64(value)
}

/// `JSON.parse` in the daemon treats an integral float as an integer — mirror
/// that on every parsed request body so later round-trips agree with JS.
fn normalize_js_numbers(value: Value) -> Value {
    match value {
        Value::Number(number) => match number.as_f64() {
            Some(real) => json_f64(real),
            None => Value::Number(number),
        },
        Value::Array(items) => Value::Array(items.into_iter().map(normalize_js_numbers).collect()),
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .map(|(key, value)| (key, normalize_js_numbers(value)))
                .collect(),
        ),
        other => other,
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// UUID-v4 shaped id, matching `randomUUID()` without a `uuid` dependency.
fn random_id() -> String {
    use std::hash::{BuildHasher, Hasher};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut bytes = [0u8; 16];
    let counter = COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
    for (chunk, seed) in bytes.chunks_mut(8).enumerate() {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(counter);
        hasher.write_u64(chunk as u64);
        hasher.write_u64(now_ms() as u64);
        hasher.write_u32(std::process::id());
        seed.copy_from_slice(&hasher.finish().to_le_bytes());
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// `{ ...value }` for an object literal spread.
fn spread_json_value_into(out: &mut Map<String, Value>, value: &Value) {
    match value {
        Value::Object(object) => {
            for (key, item) in object {
                out.insert(key.clone(), item.clone());
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                out.insert(index.to_string(), item.clone());
            }
        }
        // Object spread of a string yields one index-keyed entry per code
        // point (JavaScript indexes UTF-16 units; the divergence is documented
        // and only reachable from a bare-string body).
        Value::String(text) => {
            for (index, c) in text.chars().enumerate() {
                out.insert(index.to_string(), json!(c.to_string()));
            }
        }
        _ => {}
    }
}

fn spread_object(value: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    spread_json_value_into(&mut out, value);
    out
}

