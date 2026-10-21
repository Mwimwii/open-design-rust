//! BYOK chat proxy streaming — Rust parity for the OpenAI, Anthropic, Azure, Google, and Ollama routes.


use axum::response::sse::{Event, Sse};
use serde_json::{json, Value};
use axum::response::IntoResponse;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt as _;

use crate::egress;
use crate::routes::{api_error, AppState};

pub fn router() -> axum::routing::Router<AppState> {
    use axum::routing::post;
    axum::routing::Router::new()
        .route("/api/proxy/openai/stream", post(openai_proxy))
        .route("/api/proxy/anthropic/stream", post(anthropic_proxy))
        .route("/api/proxy/azure/stream", post(azure_proxy))
        .route("/api/proxy/google/stream", post(google_proxy))
        .route("/api/proxy/ollama/stream", post(ollama_proxy))
}

/// Parity: `rejectProxyPluginContext` in `apps/daemon/src/routes/chat.ts`.
/// Plugin runs must be resolved and pinned by `POST /api/runs`, never streamed
/// through a proxy route with a caller-supplied snapshot id.
fn reject_proxy_plugin_context(
    body: &serde_json::Map<String, Value>,
) -> Option<axum::response::Response> {
    let present = |key: &str| {
        body.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .map(|value| !value.is_empty())
            .unwrap_or(false)
    };
    if present("pluginId") || present("appliedPluginSnapshotId") {
        return Some(api_error(
            axum::http::StatusCode::CONFLICT,
            "PLUGIN_REQUIRES_DAEMON",
            "Plugin runs must go through POST /api/runs so the daemon can resolve and pin the applied plugin snapshot.",
        ));
    }
    None
}

/// Parity: `validateExternalApiBaseUrl` → `validateUserProviderBaseUrl`.
/// A blocked host answers 403 FORBIDDEN; a malformed one 400 BAD_REQUEST.
async fn validate_external_api_base_url(
    base_url: &str,
) -> Option<axum::response::Response> {
    let validated = egress::validate_user_provider_base_url(base_url).await;
    let error = validated.error?;
    let (status, code) = if validated.forbidden {
        (axum::http::StatusCode::FORBIDDEN, "FORBIDDEN")
    } else {
        (axum::http::StatusCode::BAD_REQUEST, "BAD_REQUEST")
    };
    Some(api_error(status, code, &error))
}

/// Parity: `authorizeReasoningEgress` followed by `sendReasoningEgressDenial`.
/// Absent policy means enabled.
pub(crate) fn reasoning_egress_denial(
    body: &serde_json::Map<String, Value>,
    route_kind: egress::ReasoningEgressRouteKind,
    provider: &str,
    base_url: Option<&str>,
    model: Option<&str>,
) -> Option<axum::response::Response> {
    let request = egress::ReasoningEgressRequest {
        policy: body.get("reasoningExecution").cloned(),
        route_kind,
        provider: provider.to_string(),
        resolved_base_url: base_url.map(str::to_string),
        model: model.map(str::to_string),
    };
    egress::authorize_reasoning_egress(&request)
        .map(egress::ReasoningEgressDenial::into_response)
}

/// The egress guards that run after the required-field check, in TS order:
/// resolved base URL, then reasoning policy.
async fn guard_external_egress(
    body: &serde_json::Map<String, Value>,
    provider: &str,
    base_url: &str,
    model: &str,
) -> Option<axum::response::Response> {
    if let Some(response) = validate_external_api_base_url(base_url).await {
        return Some(response);
    }
    reasoning_egress_denial(
        body,
        egress::ReasoningEgressRouteKind::Proxy,
        provider,
        Some(base_url),
        Some(model),
    )
}

async fn openai_proxy(axum::Json(body): axum::Json<Value>) -> axum::response::Response {
    let body = body.as_object().cloned().unwrap_or_default();
    if let Some(response) = reject_proxy_plugin_context(&body) {
        return response;
    }
    let base_url = body.get("baseUrl").and_then(Value::as_str).unwrap_or_default().trim().to_string();
    let api_key = body.get("apiKey").and_then(Value::as_str).unwrap_or_default().trim().to_string();
    let model = body.get("model").and_then(Value::as_str).unwrap_or_default().trim().to_string();
    if base_url.is_empty() || api_key.is_empty() || model.is_empty() {
        return api_error(axum::http::StatusCode::BAD_REQUEST, "BAD_REQUEST", "baseUrl, apiKey, and model are required");
    }
    if let Some(response) = guard_external_egress(&body, "openai", &base_url, &model).await {
        return response;
    }
    let system_prompt = body.get("systemPrompt").and_then(Value::as_str).map(str::to_string);
    let messages = body.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
    let max_tokens = body
        .get("maxTokens")
        .and_then(Value::as_i64)
        .filter(|v| *v > 0)
        .unwrap_or(8192) as u64;

    let mut payload_messages = messages;
    if let Some(sp) = system_prompt {
        if !sp.is_empty() {
            payload_messages.insert(0, json!({"role":"system","content":sp}));
        }
    }

    let payload = json!({
        "model": model,
        "messages": payload_messages,
        "max_tokens": max_tokens,
        "stream": true
    });

    // build url
    let url = match append_versioned(&base_url, "/chat/completions") {
        Ok(u) => u,
        Err(e) => {
            return api_error(axum::http::StatusCode::BAD_REQUEST, "BAD_REQUEST", &e);
        }
    };

    let (tx, rx) = mpsc::channel::<Result<Event, Box<dyn std::error::Error + Send + Sync>>>(32);

    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        if tx
            .send(Ok(Event::default().event("start").data(json!({"model": model}).to_string())))
            .await
            .is_err()
        {
            return;
        }

        let resp = client
            .post(&url)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", api_key))
            .json(&payload)
            .send()
            .await;

        let response = match resp {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(Ok(Event::default().event("error").data(json!({"message": e.to_string()}).to_string()))).await;
                let _ = tx.send(Ok(Event::default().event("end").data("{}"))).await;
                return;
            }
        };

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            let _ = tx
                .send(Ok(Event::default().event("error").data(json!({"message": format!("Upstream error: {}", status), "details": text}).to_string())))
                .await;
            let _ = tx.send(Ok(Event::default().event("end").data("{}"))).await;
            return;
        }

        let mut buffer = String::new();
        let mut ended = false;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    buffer.push_str(&String::from_utf8_lossy(&bytes));
                    while let Some(pos) = buffer.find("\n\n") {
                        let frame = buffer[..pos].to_string();
                        buffer.drain(..pos + 2);
                        for line in frame.lines() {
                            let line = line.trim();
                            if let Some(data) = line.strip_prefix("data:") {
                                let data = data.trim();
                                if data == "[DONE]" {
                                    let _ = tx.send(Ok(Event::default().event("end").data("{}"))).await;
                                    ended = true;
                                    break;
                                }
                                if let Ok(val) = serde_json::from_str::<Value>(data) {
                                    if let Some(delta) = val.get("choices").and_then(Value::as_array).and_then(|a| a.first()).and_then(|c| c.get("delta")).and_then(|d| d.get("content")).and_then(Value::as_str) {
                                        if !delta.is_empty() {
                                            let _ = tx.send(Ok(Event::default().event("delta").data(json!({"content": delta}).to_string()))).await;
                                        }
                                    }
                                }
                            }
                        }
                        if ended {
                            break;
                        }
                    }
                    if ended {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Ok(Event::default().event("error").data(json!({"message": e.to_string()}).to_string()))).await;
                    let _ = tx.send(Ok(Event::default().event("end").data("{}"))).await;
                    ended = true;
                    break;
                }
            }
        }
        if !ended {
            let _ = tx.send(Ok(Event::default().event("end").data("{}"))).await;
        }
    });

    let stream = ReceiverStream::new(rx);
    Sse::new(stream).into_response()
}

fn append_versioned(base_url: &str, suffix: &str) -> Result<String, String> {
    let mut url = url::Url::parse(base_url).map_err(|e| e.to_string())?;
    let pathname = url.path().trim_end_matches('/').to_string();
    let has_version = pathname
        .split('/')
        .any(|seg| seg.len() >= 2 && seg.starts_with('v') && seg[1..].chars().all(|c| c.is_ascii_digit()));
    let new_path = if has_version {
        format!("{}{}", pathname, suffix)
    } else {
        format!("{}/v1{}", pathname, suffix)
    };
    url.set_path(&new_path);
    url.set_query(None);
    Ok(url.to_string())
}

async fn anthropic_proxy(axum::Json(body): axum::Json<Value>) -> axum::response::Response {
    let body = body.as_object().cloned().unwrap_or_default();
    if let Some(response) = reject_proxy_plugin_context(&body) {
        return response;
    }
    let base_url = body.get("baseUrl").and_then(Value::as_str).unwrap_or_default().trim().to_string();
    let api_key = body.get("apiKey").and_then(Value::as_str).unwrap_or_default().trim().to_string();
    let model = body.get("model").and_then(Value::as_str).unwrap_or_default().trim().to_string();
    if base_url.is_empty() || api_key.is_empty() || model.is_empty() {
        return api_error(axum::http::StatusCode::BAD_REQUEST, "BAD_REQUEST", "baseUrl, apiKey, and model are required");
    }
    if let Some(response) = guard_external_egress(&body, "anthropic", &base_url, &model).await {
        return response;
    }
    let system_prompt = body.get("systemPrompt").and_then(Value::as_str).map(str::to_string);
    let messages = body.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
    let max_tokens = body
        .get("maxTokens")
        .and_then(Value::as_i64)
        .filter(|v| *v > 0)
        .unwrap_or(8192);

    let mut payload = json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": messages,
        "stream": true
    });
    if let Some(sp) = system_prompt {
        if !sp.is_empty() {
            payload["system"] = json!(sp);
        }
    }

    let url = match append_versioned(&base_url, "/messages") {
        Ok(u) => u,
        Err(e) => {
            return api_error(axum::http::StatusCode::BAD_REQUEST, "BAD_REQUEST", &e);
        }
    };

    let (tx, rx) = mpsc::channel::<Result<Event, Box<dyn std::error::Error + Send + Sync>>>(32);

    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        if tx
            .send(Ok(Event::default().event("start").data(json!({"model": model}).to_string())))
            .await
            .is_err()
        {
            return;
        }

        let resp = client
            .post(&url)
            .header("content-type", "application/json")
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&payload)
            .send()
            .await;

        let response = match resp {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(Ok(Event::default().event("error").data(json!({"message": e.to_string()}).to_string()))).await;
                let _ = tx.send(Ok(Event::default().event("end").data("{}"))).await;
                return;
            }
        };

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            let retryable = status.as_u16() == 429 || status.as_u16() >= 500;
            let _ = tx
                .send(Ok(Event::default().event("error").data(json!({"message": format!("Upstream error: {}", status), "details": text, "retryable": retryable}).to_string())))
                .await;
            let _ = tx.send(Ok(Event::default().event("end").data("{}"))).await;
            return;
        }

        let mut buffer = String::new();
        let mut ended = false;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    buffer.push_str(&String::from_utf8_lossy(&bytes));
                    while let Some(pos) = buffer.find("\n\n") {
                        let frame = buffer[..pos].to_string();
                        buffer.drain(..pos + 2);
                        let mut event = "message".to_string();
                        let mut data_lines: Vec<String> = Vec::new();
                        for line in frame.lines() {
                            let line = line.trim();
                            if let Some(ev) = line.strip_prefix("event:") {
                                event = ev.trim().to_string();
                            } else if let Some(data) = line.strip_prefix("data:") {
                                data_lines.push(data.trim().to_string());
                            }
                        }
                        let data_str = data_lines.join("\n");
                        if data_str.is_empty() {
                            continue;
                        }
                        let val: Value = match serde_json::from_str(&data_str) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        if event == "error" || val.get("type").and_then(Value::as_str) == Some("error") {
                            let message = val
                                .get("error")
                                .and_then(|e| e.get("message").and_then(Value::as_str))
                                .or_else(|| val.get("message").and_then(Value::as_str))
                                .unwrap_or("Anthropic upstream error")
                                .to_string();
                            let _ = tx.send(Ok(Event::default().event("error").data(json!({"message": message}).to_string()))).await;
                            let _ = tx.send(Ok(Event::default().event("end").data("{}"))).await;
                            ended = true;
                            break;
                        }
                        if event == "content_block_delta" {
                            if let Some(text) = val.get("delta").and_then(|d| d.get("text")).and_then(Value::as_str) {
                                if !text.is_empty() {
                                    let _ = tx.send(Ok(Event::default().event("delta").data(json!({"content": text}).to_string()))).await;
                                }
                            }
                        }
                        if event == "message_stop" {
                            let _ = tx.send(Ok(Event::default().event("end").data("{}"))).await;
                            ended = true;
                            break;
                        }
                    }
                    if ended {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Ok(Event::default().event("error").data(json!({"message": e.to_string()}).to_string()))).await;
                    let _ = tx.send(Ok(Event::default().event("end").data("{}"))).await;
                    ended = true;
                    break;
                }
            }
        }
        if !ended {
            let _ = tx.send(Ok(Event::default().event("end").data("{}"))).await;
        }
    });

    let stream = ReceiverStream::new(rx);
    Sse::new(stream).into_response()
}

async fn azure_proxy(axum::Json(body): axum::Json<Value>) -> axum::response::Response {
    let body = body.as_object().cloned().unwrap_or_default();
    if let Some(response) = reject_proxy_plugin_context(&body) {
        return response;
    }
    let base_url = body.get("baseUrl").and_then(Value::as_str).unwrap_or_default().trim().to_string();
    let api_key = body.get("apiKey").and_then(Value::as_str).unwrap_or_default().trim().to_string();
    let model = body.get("model").and_then(Value::as_str).unwrap_or_default().trim().to_string();
    if base_url.is_empty() || api_key.is_empty() || model.is_empty() {
        return api_error(axum::http::StatusCode::BAD_REQUEST, "BAD_REQUEST", "baseUrl, apiKey, and model are required");
    }
    if let Some(response) = guard_external_egress(&body, "azure", &base_url, &model).await {
        return response;
    }
    let system_prompt = body.get("systemPrompt").and_then(Value::as_str).map(str::to_string);
    let messages = body.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
    let max_tokens = body
        .get("maxTokens")
        .and_then(Value::as_i64)
        .filter(|v| *v > 0)
        .unwrap_or(8192);
    let api_version = body.get("apiVersion").and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);

    let mut payload_messages = messages;
    if let Some(sp) = system_prompt {
        if !sp.is_empty() {
            payload_messages.insert(0, json!({"role":"system","content":sp}));
        }
    }

    // Azure URL shape: versioned OpenAI path keeps /chat/completions,
    // otherwise /openai/deployments/{model}/chat/completions with api-version.
    let mut url = match url::Url::parse(&base_url) {
        Ok(u) => u,
        Err(e) => {
            return api_error(axum::http::StatusCode::BAD_REQUEST, "BAD_REQUEST", &e.to_string());
        }
    };
    let base_path = url.path().trim_end_matches('/').to_string();
    let uses_versioned = regex_versioned_openai_path(&base_path);
    let version = api_version.or_else(|| {
        if uses_versioned {
            None
        } else {
            Some("2024-10-21".to_string())
        }
    });
    let new_path = if uses_versioned {
        format!("{}/chat/completions", base_path)
    } else {
        format!("{}/openai/deployments/{}/chat/completions", base_path, urlencoding(&model))
    };
    url.set_path(&new_path);
    if let Some(v) = &version {
        url.query_pairs_mut().append_pair("api-version", v);
    }
    let url = url.to_string();

    let mut payload = json!({
        "messages": payload_messages.clone(),
        "max_tokens": max_tokens,
        "stream": true
    });
    if uses_versioned {
        payload["model"] = json!(model);
    }
    let mut retry_payload = json!({
        "messages": payload_messages,
        "max_completion_tokens": max_tokens,
        "stream": true
    });
    if uses_versioned {
        retry_payload["model"] = json!(model);
    }

    let (tx, rx) = mpsc::channel::<Result<Event, Box<dyn std::error::Error + Send + Sync>>>(32);

    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        if tx
            .send(Ok(Event::default().event("start").data(json!({"model": model}).to_string())))
            .await
            .is_err()
        {
            return;
        }

        let post = |body: Value| {
            client
                .post(&url)
                .header("content-type", "application/json")
                .header("api-key", api_key.clone())
                .json(&body)
                .send()
        };

        let mut response = match post(payload).await {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(Ok(Event::default().event("error").data(json!({"message": e.to_string()}).to_string()))).await;
                let _ = tx.send(Ok(Event::default().event("end").data("{}"))).await;
                return;
            }
        };

        if !response.status().is_success() {
            if response.status().as_u16() == 400 {
                let text = response.text().await.unwrap_or_default();
                if is_unsupported_max_tokens_error(&text) {
                    response = match post(retry_payload).await {
                        Ok(r) => r,
                        Err(e) => {
                            let _ = tx.send(Ok(Event::default().event("error").data(json!({"message": e.to_string()}).to_string()))).await;
                            let _ = tx.send(Ok(Event::default().event("end").data("{}"))).await;
                            return;
                        }
                    };
                    if !response.status().is_success() {
                        let status = response.status();
                        let text = response.text().await.unwrap_or_default();
                        let _ = tx
                            .send(Ok(Event::default().event("error").data(json!({"message": format!("Upstream error: {}", status), "details": text}).to_string())))
                            .await;
                        let _ = tx.send(Ok(Event::default().event("end").data("{}"))).await;
                        return;
                    }
                } else {
                    let _ = tx
                        .send(Ok(Event::default().event("error").data(json!({"message": "Upstream error: 400", "details": text}).to_string())))
                        .await;
                    let _ = tx.send(Ok(Event::default().event("end").data("{}"))).await;
                    return;
                }
            } else {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                let _ = tx
                    .send(Ok(Event::default().event("error").data(json!({"message": format!("Upstream error: {}", status), "details": text}).to_string())))
                    .await;
                let _ = tx.send(Ok(Event::default().event("end").data("{}"))).await;
                return;
            }
        }

        stream_openai_frames(tx, response).await;
    });

    let stream = ReceiverStream::new(rx);
    Sse::new(stream).into_response()
}

fn regex_versioned_openai_path(path: &str) -> bool {
    // /openai/vN(/...) — mirrors /\/openai\/v\d+(?:$|\/)/ in TS.
    let mut segs = path.split('/').peekable();
    while let Some(seg) = segs.next() {
        if seg == "openai" {
            if let Some(next) = segs.peek() {
                if next.len() >= 2 && next.starts_with('v') && next[1..].chars().all(|c| c.is_ascii_digit()) {
                    return true;
                }
            }
        }
    }
    false
}

fn is_unsupported_max_tokens_error(detail: &str) -> bool {
    let n = detail.to_lowercase();
    n.contains("unsupported parameter") && n.contains("max_tokens") && n.contains("max_completion_tokens")
}

fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

async fn stream_openai_frames(
    tx: mpsc::Sender<Result<Event, Box<dyn std::error::Error + Send + Sync>>>,
    response: reqwest::Response,
) {
    let mut buffer = String::new();
    let mut ended = false;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                buffer.push_str(&String::from_utf8_lossy(&bytes));
                while let Some(pos) = buffer.find("\n\n") {
                    let frame = buffer[..pos].to_string();
                    buffer.drain(..pos + 2);
                    for line in frame.lines() {
                        let line = line.trim();
                        if let Some(data) = line.strip_prefix("data:") {
                            let data = data.trim();
                            if data == "[DONE]" {
                                let _ = tx.send(Ok(Event::default().event("end").data("{}"))).await;
                                ended = true;
                                break;
                            }
                            if let Ok(val) = serde_json::from_str::<Value>(data) {
                                if let Some(msg) = extract_stream_error(&val) {
                                    let _ = tx.send(Ok(Event::default().event("error").data(json!({"message": msg}).to_string()))).await;
                                    let _ = tx.send(Ok(Event::default().event("end").data("{}"))).await;
                                    ended = true;
                                    break;
                                }
                                if let Some(delta) = extract_openai_delta(&val) {
                                    if !delta.is_empty() {
                                        let _ = tx.send(Ok(Event::default().event("delta").data(json!({"content": delta}).to_string()))).await;
                                    }
                                }
                            }
                        }
                    }
                    if ended {
                        break;
                    }
                }
                if ended {
                    break;
                }
            }
            Err(e) => {
                let _ = tx.send(Ok(Event::default().event("error").data(json!({"message": e.to_string()}).to_string()))).await;
                let _ = tx.send(Ok(Event::default().event("end").data("{}"))).await;
                ended = true;
                break;
            }
        }
    }
    if !ended {
        let _ = tx.send(Ok(Event::default().event("end").data("{}"))).await;
    }
}

fn extract_openai_delta(data: &Value) -> Option<String> {
    let first = data.get("choices")?.as_array()?.first()?;
    if let Some(s) = first.get("delta").and_then(|d| d.get("content")).and_then(Value::as_str) {
        return Some(s.to_string());
    }
    first.get("text").and_then(Value::as_str).map(str::to_string)
}

fn extract_stream_error(data: &Value) -> Option<String> {
    let err = data.get("error")?;
    if let Some(s) = err.as_str() {
        return Some(s.to_string());
    }
    if let Some(m) = err.get("message").and_then(Value::as_str) {
        return Some(m.to_string());
    }
    Some(err.to_string())
}

async fn google_proxy(axum::Json(body): axum::Json<Value>) -> axum::response::Response {
    let body = body.as_object().cloned().unwrap_or_default();
    if let Some(response) = reject_proxy_plugin_context(&body) {
        return response;
    }
    let api_key = body.get("apiKey").and_then(Value::as_str).unwrap_or_default().trim().to_string();
    let model = body.get("model").and_then(Value::as_str).unwrap_or_default().trim().to_string();
    if api_key.is_empty() || model.is_empty() {
        return api_error(axum::http::StatusCode::BAD_REQUEST, "BAD_REQUEST", "apiKey and model are required");
    }
    let base_url = body
        .get("baseUrl")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("https://generativelanguage.googleapis.com")
        .to_string();
    if let Some(response) = guard_external_egress(&body, "google", &base_url, &model).await {
        return response;
    }
    let system_prompt = body.get("systemPrompt").and_then(Value::as_str).map(str::to_string);
    let messages = body.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
    let max_tokens = body
        .get("maxTokens")
        .and_then(Value::as_i64)
        .filter(|v| *v > 0)
        .unwrap_or(8192);

    let contents: Vec<Value> = messages
        .iter()
        .map(|m| {
            let role = if m.get("role").and_then(Value::as_str) == Some("assistant") {
                "model"
            } else {
                "user"
            };
            let text = m.get("content").and_then(Value::as_str).unwrap_or_default();
            json!({"role": role, "parts": [{"text": text}]})
        })
        .collect();
    let mut payload = json!({
        "contents": contents,
        "generationConfig": {"maxOutputTokens": max_tokens}
    });
    if let Some(sp) = system_prompt {
        if !sp.is_empty() {
            payload["systemInstruction"] = json!({"parts": [{"text": sp}]});
        }
    }

    let stripped = strip_google_base(&base_url);
    let url = format!(
        "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
        stripped.trim_end_matches('/'),
        urlencoding(&model)
    );

    let (tx, rx) = mpsc::channel::<Result<Event, Box<dyn std::error::Error + Send + Sync>>>(32);

    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        if tx
            .send(Ok(Event::default().event("start").data(json!({"model": model}).to_string())))
            .await
            .is_err()
        {
            return;
        }

        let resp = client
            .post(&url)
            .header("content-type", "application/json")
            .header("x-goog-api-key", api_key)
            .json(&payload)
            .send()
            .await;

        let response = match resp {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(Ok(Event::default().event("error").data(json!({"message": e.to_string()}).to_string()))).await;
                let _ = tx.send(Ok(Event::default().event("end").data("{}"))).await;
                return;
            }
        };

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            let retryable = status.as_u16() == 429 || status.as_u16() >= 500;
            let _ = tx
                .send(Ok(Event::default().event("error").data(json!({"message": format!("Upstream error: {}", status), "details": text, "retryable": retryable}).to_string())))
                .await;
            let _ = tx.send(Ok(Event::default().event("end").data("{}"))).await;
            return;
        }

        let mut buffer = String::new();
        let mut ended = false;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    buffer.push_str(&String::from_utf8_lossy(&bytes));
                    while let Some(pos) = buffer.find("\n\n") {
                        let frame = buffer[..pos].to_string();
                        buffer.drain(..pos + 2);
                        for line in frame.lines() {
                            let line = line.trim();
                            if let Some(data) = line.strip_prefix("data:") {
                                let data = data.trim();
                                if data.is_empty() {
                                    continue;
                                }
                                let val: Value = match serde_json::from_str(data) {
                                    Ok(v) => v,
                                    Err(_) => continue,
                                };
                                if let Some(msg) = extract_stream_error(&val) {
                                    let _ = tx.send(Ok(Event::default().event("error").data(json!({"message": format!("Gemini error: {}", msg)}).to_string()))).await;
                                    let _ = tx.send(Ok(Event::default().event("end").data("{}"))).await;
                                    ended = true;
                                    break;
                                }
                                if let Some(delta) = extract_gemini_text(&val) {
                                    if !delta.is_empty() {
                                        let _ = tx.send(Ok(Event::default().event("delta").data(json!({"content": delta}).to_string()))).await;
                                    }
                                }
                                if let Some(block) = extract_gemini_block(&val) {
                                    let _ = tx.send(Ok(Event::default().event("error").data(json!({"message": block}).to_string()))).await;
                                    let _ = tx.send(Ok(Event::default().event("end").data("{}"))).await;
                                    ended = true;
                                    break;
                                }
                            }
                        }
                        if ended {
                            break;
                        }
                    }
                    if ended {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Ok(Event::default().event("error").data(json!({"message": e.to_string()}).to_string()))).await;
                    let _ = tx.send(Ok(Event::default().event("end").data("{}"))).await;
                    ended = true;
                    break;
                }
            }
        }
        if !ended {
            let _ = tx.send(Ok(Event::default().event("end").data("{}"))).await;
        }
    });

    let stream = ReceiverStream::new(rx);
    Sse::new(stream).into_response()
}

fn strip_google_base(base_url: &str) -> String {
    let Ok(u) = url::Url::parse(base_url) else {
        return base_url.trim_end_matches('/').to_string();
    };
    let origin = format!("{}://{}", u.scheme(), u.host_str().unwrap_or(""));
    let mut path = u.path().trim_end_matches('/').to_string();
    for suffix in ["/v1beta", "/v1"] {
        if let Some(stripped) = path.strip_suffix(suffix) {
            path = stripped.to_string();
            break;
        }
    }
    // strip trailing /vNbeta forms
    if path.is_empty() {
        return origin;
    }
    format!("{}{}", origin, path.trim_end_matches('/'))
}

fn extract_gemini_text(data: &Value) -> Option<String> {
    let candidates = data.get("candidates")?.as_array()?;
    let parts = candidates.first()?.get("content")?.get("parts")?.as_array()?;
    let mut out = String::new();
    for part in parts {
        if let Some(t) = part.get("text").and_then(Value::as_str) {
            out.push_str(t);
        }
    }
    Some(out)
}

static BENIGN_GEMINI_FINISH_REASONS: &[&str] = &["", "STOP", "MAX_TOKENS", "FINISH_REASON_UNSPECIFIED"];

fn extract_gemini_block(data: &Value) -> Option<String> {
    if let Some(feedback) = data.get("promptFeedback") {
        if let Some(reason) = feedback.get("blockReason").and_then(Value::as_str) {
            if !reason.is_empty() {
                let tail = feedback
                    .get("blockReasonMessage")
                    .and_then(Value::as_str)
                    .filter(|m| !m.is_empty())
                    .map(|m| format!(" — {}", m))
                    .unwrap_or_default();
                return Some(format!("Gemini blocked the prompt ({}){}.", reason, tail));
            }
        }
    }
    let candidates = data.get("candidates")?.as_array()?;
    for candidate in candidates {
        let reason = candidate.get("finishReason").and_then(Value::as_str).unwrap_or("");
        if BENIGN_GEMINI_FINISH_REASONS.contains(&reason) {
            continue;
        }
        let tail = candidate
            .get("finishMessage")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
            .map(|m| format!(" — {}", m))
            .unwrap_or_default();
        return Some(format!("Gemini stopped the response ({}){}.", reason, tail));
    }
    None
}

async fn ollama_proxy(axum::Json(body): axum::Json<Value>) -> axum::response::Response {
    let body = body.as_object().cloned().unwrap_or_default();
    if let Some(response) = reject_proxy_plugin_context(&body) {
        return response;
    }
    let api_key = body.get("apiKey").and_then(Value::as_str).unwrap_or_default().trim().to_string();
    let model = body.get("model").and_then(Value::as_str).unwrap_or_default().trim().to_string();
    if api_key.is_empty() || model.is_empty() {
        return api_error(axum::http::StatusCode::BAD_REQUEST, "BAD_REQUEST", "apiKey and model are required");
    }
    let base_url = body
        .get("baseUrl")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("https://ollama.com")
        .to_string();
    if let Some(response) = guard_external_egress(&body, "ollama", &base_url, &model).await {
        return response;
    }
    let system_prompt = body.get("systemPrompt").and_then(Value::as_str).map(str::to_string);
    let messages = body.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
    let max_tokens = body.get("maxTokens").and_then(Value::as_i64).filter(|v| *v > 0);

    let mut payload_messages = messages;
    if let Some(sp) = system_prompt {
        if !sp.is_empty() {
            payload_messages.insert(0, json!({"role":"system","content":sp}));
        }
    }

    let clean = base_url.trim_end_matches('/').to_string();
    let clean = clean.strip_suffix("/api").unwrap_or(&clean).trim_end_matches('/').to_string();
    let url = format!("{}/api/chat", clean);

    let mut payload = json!({
        "model": model,
        "messages": payload_messages,
        "stream": true
    });
    if let Some(n) = max_tokens {
        payload["options"] = json!({"num_predict": n});
    }

    let (tx, rx) = mpsc::channel::<Result<Event, Box<dyn std::error::Error + Send + Sync>>>(32);

    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        if tx
            .send(Ok(Event::default().event("start").data(json!({"model": model}).to_string())))
            .await
            .is_err()
        {
            return;
        }

        let resp = client
            .post(&url)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", api_key))
            .json(&payload)
            .send()
            .await;

        let response = match resp {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(Ok(Event::default().event("error").data(json!({"message": e.to_string()}).to_string()))).await;
                let _ = tx.send(Ok(Event::default().event("end").data("{}"))).await;
                return;
            }
        };

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            let retryable = status.as_u16() == 429 || status.as_u16() >= 500;
            let _ = tx
                .send(Ok(Event::default().event("error").data(json!({"message": format!("Upstream error: {}", status), "details": text, "retryable": retryable}).to_string())))
                .await;
            let _ = tx.send(Ok(Event::default().event("end").data("{}"))).await;
            return;
        }

        // Ollama streams newline-delimited JSON objects, not SSE frames.
        let mut buffer = String::new();
        let mut ended = false;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    buffer.push_str(&String::from_utf8_lossy(&bytes));
                    while let Some(pos) = buffer.find('\n') {
                        let line = buffer[..pos].trim().to_string();
                        buffer.drain(..pos + 1);
                        if line.is_empty() {
                            continue;
                        }
                        let val: Value = match serde_json::from_str(&line) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        if val.get("done").and_then(Value::as_bool) == Some(true) {
                            let _ = tx.send(Ok(Event::default().event("end").data("{}"))).await;
                            ended = true;
                            break;
                        }
                        if let Some(content) = val.get("message").and_then(|m| m.get("content")).and_then(Value::as_str) {
                            if !content.is_empty() {
                                let _ = tx.send(Ok(Event::default().event("delta").data(json!({"content": content}).to_string()))).await;
                            }
                        }
                    }
                    if ended {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Ok(Event::default().event("error").data(json!({"message": e.to_string()}).to_string()))).await;
                    let _ = tx.send(Ok(Event::default().event("end").data("{}"))).await;
                    ended = true;
                    break;
                }
            }
        }
        if !ended {
            let _ = tx.send(Ok(Event::default().event("end").data("{}"))).await;
        }
    });

    let stream = ReceiverStream::new(rx);
    Sse::new(stream).into_response()
}
