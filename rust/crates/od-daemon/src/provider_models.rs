//! Provider model discovery — Rust parity port of
//! `apps/daemon/src/integrations/provider-models.ts`.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::Client;
use serde_json::{json, Value};
use url::Url;

const PROVIDER_MODELS_TIMEOUT_MS: u64 = 12_000;

#[derive(Debug, Clone)]
struct ProviderModel {
    id: String,
    label: String,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn status_to_kind(status: u16) -> &'static str {
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

fn unique_models(models: Vec<ProviderModel>) -> Vec<ProviderModel> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for m in models {
        let id = m.id.trim().to_string();
        if id.is_empty() || seen.contains(&id) {
            continue;
        }
        seen.insert(id.clone());
        let label = if m.label.trim().is_empty() {
            id.clone()
        } else {
            m.label.trim().to_string()
        };
        out.push(ProviderModel { id, label });
    }
    out
}

fn is_openai_chat_model_id(id: &str) -> bool {
    let n = id.trim().to_lowercase();
    if n.is_empty() {
        return false;
    }
    if n.contains("embedding")
        || n.contains("moderation")
        || n.contains("rerank")
        || n.contains("whisper")
        || n.contains("tts")
        || n.contains("transcribe")
        || n.contains("speech")
        || n.contains("image")
        || n.contains("video")
        || n.contains("dall-e")
        || n.contains("stable-diffusion")
        || n.contains("flux")
    {
        return false;
    }
    // bge embedding family guard.
    if regex_like_bge(&n) {
        return false;
    }
    // media-generation guard
    if n.contains("wan") && regex_like_media(&n) {
        return false;
    }
    !regex_like_media_suffix(&n)
}

fn regex_like_bge(n: &str) -> bool {
    // simple contains bge with boundary-ish check
    n.contains("bge") && (n.starts_with("bge") || n.contains("/bge") || n.contains("_bge") || n.contains("-bge"))
}

fn regex_like_media(n: &str) -> bool {
    n.contains("t2v") || n.contains("i2v") || n.contains("v2v")
}

fn regex_like_media_suffix(n: &str) -> bool {
    // crude suffix media token guard
    n.ends_with("tts") || n.ends_with("asr") || n.ends_with("ocr")
}

fn extract_openai_models(data: &Value) -> Vec<ProviderModel> {
    let items = data.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut out = Vec::new();
    for item in items {
        let id = item.get("id").and_then(Value::as_str).unwrap_or_default();
        if id.is_empty() || !is_openai_chat_model_id(id) {
            continue;
        }
        out.push(ProviderModel {
            id: id.to_string(),
            label: id.to_string(),
        });
    }
    unique_models(out)
}

fn extract_anthropic_models(data: &Value) -> Vec<ProviderModel> {
    let items = data.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut out = Vec::new();
    for item in items {
        let id = item.get("id").and_then(Value::as_str).unwrap_or_default();
        if id.is_empty() {
            continue;
        }
        let label = item
            .get("display_name")
            .or_else(|| item.get("displayName"))
            .and_then(Value::as_str)
            .unwrap_or(id)
            .to_string();
        out.push(ProviderModel {
            id: id.to_string(),
            label,
        });
    }
    unique_models(out)
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

fn provider_models_url(protocol: &str, base_url: &str, api_key: &str) -> Result<String, String> {
    match protocol {
        "aihubmix" => {
            // derive origin
            let origin = Url::parse(base_url)
                .map(|u| format!("{}://{}", u.scheme(), u.host_str().unwrap_or("")))
                .unwrap_or_else(|_| "https://aihubmix.com".to_string());
            Ok(format!("{}/api/v1/models?type=llm", origin.trim_end_matches('/')))
        }
        "openai" | "senseaudio" => append_versioned_api_path(base_url, "/models"),
        "anthropic" => {
            let mut url = Url::parse(&append_versioned_api_path(base_url, "/models")?).map_err(|e| e.to_string())?;
            url.query_pairs_mut().append_pair("limit", "1000");
            Ok(url.to_string())
        }
        "google" => {
            // Google base url origin + v1beta/models?key=...
            let u = Url::parse(base_url).map_err(|e| e.to_string())?;
            let mut base = format!("{}://{}", u.scheme(), u.host_str().unwrap_or(""));
            // strip trailing version-ish path
            let path = u.path().trim_end_matches('/').to_string();
            let path = path.replace("/v1", "").replace("/v1beta", "");
            base = format!("{}{}", base, path);
            base = base.trim_end_matches('/').to_string();
            Ok(format!("{}/v1beta/models?key={}", base, urlencoding(api_key)))
        }
        _ => Err(format!("Unsupported protocol: {}", protocol)),
    }
}

fn provider_models_headers(protocol: &str, api_key: &str) -> Vec<(&'static str, String)> {
    let mut h = Vec::new();
    match protocol {
        "openai" | "senseaudio" => {
            if !api_key.trim().is_empty() {
                h.push(("authorization", format!("Bearer {}", api_key)));
            }
        }
        "aihubmix" => {
            if !api_key.trim().is_empty() {
                h.push(("authorization", format!("Bearer {}", api_key)));
                h.push(("APP-Code", "DMCY9912".to_string()));
            }
        }
        "anthropic" => {
            h.push(("x-api-key", api_key.to_string()));
            h.push(("anthropic-version", "2023-06-01".to_string()));
        }
        _ => {}
    }
    h
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

fn validate_base_url(base_url: &str) -> Result<Url, String> {
    let u = Url::parse(base_url).map_err(|e| e.to_string())?;
    if u.scheme() != "http" && u.scheme() != "https" {
        return Err("invalid scheme".to_string());
    }
    if u.host_str().is_none() || u.host_str().unwrap().is_empty() {
        return Err("missing host".to_string());
    }
    Ok(u)
}

pub async fn list_provider_models(body: &Value) -> Value {
    let start = Instant::now();
    let protocol = body.get("protocol").and_then(Value::as_str).unwrap_or_default();
    let base_url = body.get("baseUrl").and_then(Value::as_str).unwrap_or_default();
    let api_key = body.get("apiKey").and_then(Value::as_str).unwrap_or_default();
    let api_key_required = protocol != "aihubmix" && protocol != "bedrock";

    if (api_key_required && api_key.trim().is_empty()) || base_url.trim().is_empty() {
        return json!({
            "ok": false,
            "kind": "invalid_base_url",
            "latencyMs": 0,
            "detail": if api_key_required { "baseUrl and apiKey are required" } else { "baseUrl is required" }
        });
    }

    let _validated = match validate_base_url(base_url) {
        Ok(u) => u,
        Err(e) => {
            return json!({
                "ok": false,
                "kind": "invalid_base_url",
                "latencyMs": 0,
                "detail": e
            });
        }
    };

    if protocol == "bedrock" {
        let models = vec![
            json!({"id":"anthropic.claude-3-5-sonnet-20241022-v2:0","label":"Claude 3.5 Sonnet v2"}),
            json!({"id":"anthropic.claude-3-5-haiku-20241022-v1:0","label":"Claude 3.5 Haiku"}),
            json!({"id":"anthropic.claude-3-haiku-20240307-v1:0","label":"Claude 3 Haiku"}),
            json!({"id":"amazon.nova-pro-v1:0","label":"Amazon Nova Pro"}),
            json!({"id":"amazon.nova-lite-v1:0","label":"Amazon Nova Lite"}),
            json!({"id":"amazon.nova-micro-v1:0","label":"Amazon Nova Micro"}),
        ];
        return json!({
            "ok": true,
            "kind": "success",
            "latencyMs": now_ms() - now_ms() + (start.elapsed().as_millis() as i64),
            "models": models
        });
    }

    if protocol == "azure" {
        return json!({
            "ok": false,
            "kind": "unsupported_protocol",
            "latencyMs": start.elapsed().as_millis() as i64,
            "detail": "Azure OpenAI deployment discovery is not supported from the inference endpoint."
        });
    }

    let url = match provider_models_url(protocol, base_url, api_key) {
        Ok(u) => u,
        Err(e) => {
            return json!({
                "ok": false,
                "kind": "unsupported_protocol",
                "latencyMs": start.elapsed().as_millis() as i64,
                "detail": e
            });
        }
    };

    let client = Client::builder()
        .timeout(Duration::from_millis(PROVIDER_MODELS_TIMEOUT_MS))
        .build()
        .unwrap_or_else(|_| Client::new());

    let mut req = client.get(&url);
    for (k, v) in provider_models_headers(protocol, api_key) {
        req = req.header(k, v);
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            return json!({
                "ok": false,
                "kind": network_kind(&e),
                "latencyMs": start.elapsed().as_millis() as i64,
                "detail": e.to_string()
            });
        }
    };

    let status = resp.status().as_u16();
    let latency_ms = start.elapsed().as_millis() as i64;
    let raw = resp.text().await.unwrap_or_default();
    let data: Value = serde_json::from_str(&raw).unwrap_or(json!({}));

    if !status_is_success(status) {
        let detail = extract_detail(&data, &raw);
        return json!({
            "ok": false,
            "kind": status_to_kind(status),
            "latencyMs": latency_ms,
            "status": status,
            "detail": detail
        });
    }

    // parse error handling: if raw not empty and parse failed, return unknown
    if !raw.trim().is_empty() && data == json!({}) && raw.trim() != "{}" {
        // crude parse check
        if serde_json::from_str::<Value>(&raw).is_err() {
            return json!({
                "ok": false,
                "kind": "unknown",
                "latencyMs": latency_ms,
                "status": status,
                "detail": raw.trim().chars().take(240).collect::<String>()
            });
        }
    }

    let models = if protocol == "aihubmix" {
        // parseAIHubMixCatalog chatOnly
        parse_aihubmix_catalog(&data, true)
    } else if protocol == "anthropic" {
        extract_anthropic_models(&data)
    } else if protocol == "google" {
        // Google returns {models: [{name, baseModelId, displayName, supportedGenerationMethods}]}
        extract_google_models(&data)
    } else {
        extract_openai_models(&data)
    };

    let model_json: Vec<Value> = models
        .into_iter()
        .map(|m| json!({"id": m.id, "label": m.label}))
        .collect();

    if model_json.is_empty() {
        return json!({
            "ok": false,
            "kind": "no_models",
            "latencyMs": latency_ms,
            "status": status,
            "detail": "Provider returned no usable text-generation models."
        });
    }

    json!({
        "ok": true,
        "kind": "success",
        "latencyMs": latency_ms,
        "status": status,
        "models": model_json
    })
}

fn status_is_success(status: u16) -> bool {
    (200..300).contains(&status)
}

fn extract_detail(data: &Value, raw: &str) -> String {
    if let Some(s) = data.get("error").and_then(Value::as_str) {
        return s.to_string();
    }
    if let Some(err) = data.get("error") {
        if let Some(msg) = err.get("message").and_then(Value::as_str) {
            if !msg.trim().is_empty() {
                return msg.to_string();
            }
        }
    }
    if let Some(msg) = data.get("message").and_then(Value::as_str) {
        if !msg.trim().is_empty() {
            return msg.to_string();
        }
    }
    raw.trim().chars().take(240).collect()
}

fn parse_aihubmix_catalog(data: &Value, chat_only: bool) -> Vec<ProviderModel> {
    let rows = data.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for row in rows {
        let id = row.get("model_id").and_then(Value::as_str).unwrap_or_default();
        if id.is_empty() || !seen.insert(id.to_string()) {
            continue;
        }
        if chat_only {
            let types = row.get("types").and_then(Value::as_str).unwrap_or_default();
            let media = ["image_generation", "video", "tts"];
            let mut skip = false;
            for t in types.split(',') {
                let t = t.trim().to_lowercase();
                if media.contains(&t.as_str()) {
                    skip = true;
                    break;
                }
            }
            if skip {
                continue;
            }
        }
        let name = row.get("model_name").and_then(Value::as_str).unwrap_or_default();
        let label = if name.trim().is_empty() {
            id.to_string()
        } else {
            name.to_string()
        };
        out.push(ProviderModel {
            id: id.to_string(),
            label,
        });
    }
    unique_models(out)
}

fn extract_google_models(data: &Value) -> Vec<ProviderModel> {
    let items = data.get("models").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut out = Vec::new();
    for item in items {
        let methods = item
            .get("supportedGenerationMethods")
            .or_else(|| item.get("supported_actions"))
            .and_then(Value::as_array);
        let supports = methods
            .map(|m| m.iter().any(|v| v.as_str() == Some("generateContent")))
            .unwrap_or(false);
        if !supports {
            continue;
        }
        let raw_name = item.get("name").and_then(Value::as_str);
        let raw_base = item.get("baseModelId").and_then(Value::as_str);
        let id = if let Some(b) = raw_base {
            if !b.trim().is_empty() {
                b.trim().to_string()
            } else {
                normalize_google_model_id(raw_name.unwrap_or_default())
            }
        } else {
            normalize_google_model_id(raw_name.unwrap_or_default())
        };
        if id.is_empty() {
            continue;
        }
        let label = item
            .get("displayName")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&id)
            .to_string();
        out.push(ProviderModel { id, label });
    }
    unique_models(out)
}

fn normalize_google_model_id(model: &str) -> String {
    let t = model.trim();
    if let Some(rest) = t.strip_prefix("models/") {
        rest.to_string()
    } else {
        t.to_string()
    }
}
