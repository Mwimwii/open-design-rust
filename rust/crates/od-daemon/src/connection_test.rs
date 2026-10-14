//! Connection testing helpers — Rust parity port for `connectionTest.ts`
//! (provider mode only; agent-mode runtime definitions are not ported).

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::Client;
use serde_json::{json, Value};
use url::Url;

const PROVIDER_TIMEOUT_MS: u64 = 15_000;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn status_kind(status: u16) -> &'static str {
    match status {
        401 => "auth_failed",
        403 => "forbidden",
        404 => "invalid_base_url",
        429 => "rate_limited",
        s if s >= 500 => "upstream_unavailable",
        _ => "unknown",
    }
}

fn network_kind(err: &reqwest::Error) -> &'static str {
    if err.is_timeout() {
        "timeout"
    } else if err.is_connect() {
        "invalid_base_url"
    } else {
        "unknown"
    }
}

fn validate_base_url(base_url: &str) -> bool {
    Url::parse(base_url)
        .map(|u| (u.scheme() == "http" || u.scheme() == "https") && u.host_str().map(|h| !h.is_empty()).unwrap_or(false))
        .unwrap_or(false)
}

fn append_versioned_api_path(base_url: &str, suffix: &str) -> Result<String, String> {
    let mut url = Url::parse(base_url).map_err(|e| e.to_string())?;
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

fn google_base(base_url: &str) -> String {
    let Ok(u) = Url::parse(base_url) else {
        return base_url.trim_end_matches('/').to_string();
    };
    let mut path = u.path().trim_end_matches('/').to_string();
    path = path.replace("/v1", "").replace("/v1beta", "");
    let origin = format!("{}://{}", u.scheme(), u.host_str().unwrap_or(""));
    format!("{}{}", origin, path.trim_end_matches('/'))
}

pub async fn test_provider_connection(body: &Value) -> Value {
    let start = Instant::now();
    let protocol = body.get("protocol").and_then(Value::as_str).unwrap_or_default();
    let base_url = body.get("baseUrl").and_then(Value::as_str).unwrap_or_default().trim();
    let api_key = body.get("apiKey").and_then(Value::as_str).unwrap_or_default().trim();
    let model = body.get("model").and_then(Value::as_str).unwrap_or_default().trim().to_string();
    let api_version = body.get("apiVersion").and_then(Value::as_str).map(str::to_string);

    if base_url.is_empty() || !validate_base_url(base_url) {
        return json!({
            "ok": false,
            "kind": "invalid_base_url",
            "latencyMs": now_ms() - now_ms() + (start.elapsed().as_millis() as i64),
            "model": model,
            "detail": "baseUrl is invalid".to_string()
        });
    }

    if (protocol != "bedrock" && api_key.is_empty()) || model.is_empty() {
        return json!({
            "ok": false,
            "kind": "invalid_base_url",
            "latencyMs": start.elapsed().as_millis() as i64,
            "model": model,
            "detail": "required fields missing".to_string()
        });
    }

    let client = Client::builder()
        .timeout(Duration::from_millis(PROVIDER_TIMEOUT_MS))
        .build()
        .unwrap_or_else(|_| Client::new());

    let url: String;
    let mut headers: Vec<(&str, String)> = Vec::new();
    let mut req_body = json!({});

    match protocol {
        "openai" | "senseaudio" | "aihubmix" => {
            match append_versioned_api_path(base_url, "/chat/completions") {
                Ok(u) => url = u,
                Err(e) => {
                    return json!({
                        "ok": false,
                        "kind": "invalid_base_url",
                        "latencyMs": start.elapsed().as_millis() as i64,
                        "model": model,
                        "detail": e
                    });
                }
            }
            headers.push(("content-type", "application/json".to_string()));
            if !api_key.is_empty() {
                headers.push(("authorization", format!("Bearer {}", api_key)));
            }
            if protocol == "aihubmix" {
                headers.push(("APP-Code", "DMCY9912".to_string()));
            }
            req_body = json!({
                "model": model,
                "messages": [{"role":"user","content":"hi"}],
                "max_tokens": 16,
                "stream": false
            });
        }
        "anthropic" => {
            match append_versioned_api_path(base_url, "/messages") {
                Ok(u) => url = u,
                Err(e) => {
                    return json!({
                        "ok": false,
                        "kind": "invalid_base_url",
                        "latencyMs": start.elapsed().as_millis() as i64,
                        "model": model,
                        "detail": e
                    });
                }
            }
            headers.push(("content-type", "application/json".to_string()));
            headers.push(("x-api-key", api_key.to_string()));
            headers.push(("anthropic-version", "2023-06-01".to_string()));
            req_body = json!({
                "model": model,
                "messages": [{"role":"user","content":"hi"}],
                "max_tokens": 16
            });
        }
        "azure" => {
            let api_v = api_version.as_deref().unwrap_or("2024-02-01");
            match append_versioned_api_path(base_url, "/chat/completions") {
                Ok(mut u) => {
                    if let Ok(mut parsed) = Url::parse(&u) {
                        parsed.query_pairs_mut().append_pair("api-version", api_v);
                        u = parsed.to_string();
                    }
                    url = u;
                }
                Err(e) => {
                    return json!({
                        "ok": false,
                        "kind": "invalid_base_url",
                        "latencyMs": start.elapsed().as_millis() as i64,
                        "model": model,
                        "detail": e
                    });
                }
            }
            headers.push(("content-type", "application/json".to_string()));
            headers.push(("api-key", api_key.to_string()));
            req_body = json!({
                "model": model,
                "messages": [{"role":"user","content":"hi"}],
                "max_tokens": 16,
                "stream": false
            });
        }
        "google" => {
            url = format!("{}/v1beta/models/{}:generateContent?key={}", google_base(base_url), model, api_key);
            headers.push(("content-type", "application/json".to_string()));
            req_body = json!({
                "contents": [{"role":"user","parts":[{"text":"hi"}]}]
            });
        }
        "ollama" => {
            let u = format!("{}/api/chat", base_url.trim_end_matches('/'));
            url = u;
            headers.push(("content-type", "application/json".to_string()));
            req_body = json!({
                "model": model,
                "messages": [{"role":"user","content":"hi"}],
                "stream": false
            });
        }
        _ => {
            return json!({
                "ok": false,
                "kind": "unsupported_protocol",
                "latencyMs": start.elapsed().as_millis() as i64,
                "model": model,
                "detail": format!("Unknown protocol: {}", protocol)
            });
        }
    }

    let mut req = client.post(&url);
    for (k, v) in headers {
        req = req.header(k, v);
    }
    req = req.json(&req_body);

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            return json!({
                "ok": false,
                "kind": network_kind(&e),
                "latencyMs": start.elapsed().as_millis() as i64,
                "model": model,
                "detail": e.to_string()
            });
        }
    };

    let status = resp.status().as_u16();
    let latency = start.elapsed().as_millis() as i64;
    let raw = resp.text().await.unwrap_or_default();
    let data: Value = serde_json::from_str(&raw).unwrap_or(json!({}));

    if !(200..300).contains(&status) {
        let detail = data
            .get("error")
            .and_then(|e| e.get("message").and_then(Value::as_str))
            .or_else(|| data.get("error").and_then(Value::as_str))
            .or_else(|| data.get("message").and_then(Value::as_str))
            .map(|s| s.to_string())
            .unwrap_or_else(|| raw.trim().chars().take(240).collect());
        return json!({
            "ok": false,
            "kind": status_kind(status),
            "latencyMs": latency,
            "model": model,
            "status": status,
            "detail": detail
        });
    }

    // basic sample extraction
    let sample = extract_sample(protocol, &data);
    if sample.is_empty() && !is_success_shape(protocol, &data) {
        return json!({
            "ok": false,
            "kind": "unknown",
            "latencyMs": latency,
            "model": model,
            "status": status,
            "detail": "unexpected response".to_string()
        });
    }
    let sample = if sample.is_empty() {
        "valid completion".to_string()
    } else {
        sample
    };

    json!({
        "ok": true,
        "kind": "success",
        "latencyMs": latency,
        "model": model,
        "status": status,
        "sample": sample
    })
}

fn extract_sample(protocol: &str, data: &Value) -> String {
    match protocol {
        "google" => {
            data.get("candidates")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|c| c.get("content"))
                .and_then(|c| c.get("parts"))
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|p| p.get("text"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        }
        _ => {
            data.get("choices")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|c| {
                    c.get("message")
                        .and_then(|m| m.get("content"))
                        .and_then(Value::as_str)
                        .or_else(|| c.get("delta").and_then(|d| d.get("content")).and_then(Value::as_str))
                })
                .unwrap_or_default()
                .to_string()
        }
    }
}

fn is_success_shape(protocol: &str, data: &Value) -> bool {
    match protocol {
        "google" => data.get("candidates").is_some(),
        "ollama" => data.get("message").is_some(),
        _ => data.get("choices").is_some(),
    }
}
