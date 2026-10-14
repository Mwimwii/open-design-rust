//! BYOK chat proxy streaming — Rust parity for the OpenAI and Anthropic routes.


use axum::response::sse::{Event, Sse};
use serde_json::{json, Value};
use axum::response::IntoResponse;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt as _;

use crate::routes::{api_error, AppState};

pub fn router() -> axum::routing::Router<AppState> {
    use axum::routing::post;
    axum::routing::Router::new()
        .route("/api/proxy/openai/stream", post(openai_proxy))
        .route("/api/proxy/anthropic/stream", post(anthropic_proxy))
}

async fn openai_proxy(axum::Json(body): axum::Json<Value>) -> axum::response::Response {
    let body = body.as_object().cloned().unwrap_or_default();
    let base_url = body.get("baseUrl").and_then(Value::as_str).unwrap_or_default().trim().to_string();
    let api_key = body.get("apiKey").and_then(Value::as_str).unwrap_or_default().trim().to_string();
    let model = body.get("model").and_then(Value::as_str).unwrap_or_default().trim().to_string();
    if base_url.is_empty() || api_key.is_empty() || model.is_empty() {
        return api_error(axum::http::StatusCode::BAD_REQUEST, "BAD_REQUEST", "baseUrl, apiKey, and model are required");
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
    let base_url = body.get("baseUrl").and_then(Value::as_str).unwrap_or_default().trim().to_string();
    let api_key = body.get("apiKey").and_then(Value::as_str).unwrap_or_default().trim().to_string();
    let model = body.get("model").and_then(Value::as_str).unwrap_or_default().trim().to_string();
    if base_url.is_empty() || api_key.is_empty() || model.is_empty() {
        return api_error(axum::http::StatusCode::BAD_REQUEST, "BAD_REQUEST", "baseUrl, apiKey, and model are required");
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
