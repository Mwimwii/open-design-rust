//! Daemon-side OAuth 2.1 client for HTTP / SSE MCP servers — Rust parity
//! port of `apps/daemon/src/mcp-oauth.ts`.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

// ── Types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProtectedResourceMetadata {
    pub resource: Option<String>,
    #[serde(rename = "authorization_servers")]
    pub authorization_servers: Option<Vec<String>>,
    #[serde(rename = "scopes_supported")]
    pub scopes_supported: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AuthorizationServerMetadata {
    pub issuer: Option<String>,
    #[serde(rename = "authorization_endpoint")]
    pub authorization_endpoint: String,
    #[serde(rename = "token_endpoint")]
    pub token_endpoint: String,
    #[serde(rename = "registration_endpoint")]
    pub registration_endpoint: Option<String>,
    #[serde(rename = "scopes_supported")]
    pub scopes_supported: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredClient {
    #[serde(rename = "authServerIssuer")]
    pub auth_server_issuer: String,
    #[serde(rename = "redirectUri")]
    pub redirect_uri: String,
    #[serde(rename = "clientId")]
    pub client_id: String,
    #[serde(rename = "clientSecret", skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(rename = "registeredAt")]
    pub registered_at: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OAuthTokenResponse {
    pub access_token: String,
    pub token_type: Option<String>,
    pub expires_in: Option<u64>,
    pub refresh_token: Option<String>,
    pub scope: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PendingAuthState {
    pub server_id: String,
    pub auth_server_issuer: String,
    pub token_endpoint: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub redirect_uri: String,
    pub code_verifier: String,
    pub scope: Option<String>,
    pub resource_url: Option<String>,
    pub created_at: u64,
}

// ── PKCE + state helpers ───────────────────────────────────────────────

const VERIFIER_LEN: usize = 64;

fn base64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn generate_code_verifier() -> String {
    let mut buf = vec![0u8; VERIFIER_LEN];
    rand::thread_rng().fill_bytes(&mut buf);
    base64url(&buf)
}

pub fn derive_code_challenge(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    base64url(&hasher.finalize())
}

pub fn generate_state() -> String {
    let mut buf = vec![0u8; 32];
    rand::thread_rng().fill_bytes(&mut buf);
    base64url(&buf)
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ── Discovery ──────────────────────────────────────────────────────────

pub async fn discover_protected_resource(
    resource_url: &str,
    client: &reqwest::Client,
) -> Option<ProtectedResourceMetadata> {
    let parsed = Url::parse(resource_url).ok()?;
    let base = format!("{}://{}", parsed.scheme(), parsed.host_str()?);
    let path = parsed.path().trim_end_matches('/').to_string();
    // candidates
    let candidate1 = if path.is_empty() {
        format!("{}/.well-known/oauth-protected-resource", base)
    } else {
        format!("{}/.well-known/oauth-protected-resource{}", base, path)
    };
    let candidate2 = format!("{}/.well-known/oauth-protected-resource", base);
    for url in [candidate1, candidate2] {
        if let Some(json) = fetch_json::<ProtectedResourceMetadata>(client, &url).await {
            return Some(json);
        }
    }
    None
}

pub async fn discover_auth_server(
    issuer: &str,
    client: &reqwest::Client,
) -> Option<AuthorizationServerMetadata> {
    let parsed = Url::parse(issuer).ok()?;
    let base = format!("{}://{}", parsed.scheme(), parsed.host_str().unwrap_or(""));
    let trimmed = parsed.path().trim_end_matches('/');
    let candidates = [
        format!("{}/.well-known/oauth-authorization-server{}", base, trimmed),
        format!("{}/.well-known/openid-configuration{}", base, trimmed),
        format!("{}/.well-known/oauth-authorization-server", base),
        format!("{}/.well-known/openid-configuration", base),
    ];
    for url in candidates {
        if let Some(json) = fetch_json::<AuthorizationServerMetadata>(client, &url).await {
            if !json.authorization_endpoint.is_empty() && !json.token_endpoint.is_empty() {
                let mut m = json;
                if m.issuer.is_none() {
                    m.issuer = Some(issuer.to_string());
                }
                return Some(m);
            }
        }
    }
    None
}

async fn fetch_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
) -> Option<T> {
    let res = client.get(url).header("accept", "application/json").send().await.ok()?;
    if !res.status().is_success() {
        return None;
    }
    res.json::<T>().await.ok()
}

// ── Client cache ───────────────────────────────────────────────────────

#[derive(Default, Serialize, Deserialize)]
struct ClientCacheFile {
    clients: Vec<RegisteredClient>,
}

fn clients_file(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("mcp-oauth-clients.json")
}

fn read_client_cache(data_dir: &Path) -> ClientCacheFile {
    let path = clients_file(data_dir);
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return ClientCacheFile::default(),
    };
    let parsed: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return ClientCacheFile::default(),
    };
    let mut clients = Vec::new();
    if let Some(arr) = parsed.get("clients").and_then(|v| v.as_array()) {
        for v in arr {
            if let Ok(c) = serde_json::from_value::<RegisteredClient>(v.clone()) {
                // validation: required fields
                if !c.auth_server_issuer.is_empty()
                    && !c.redirect_uri.is_empty()
                    && !c.client_id.is_empty()
                {
                    clients.push(c);
                }
            }
        }
    }
    ClientCacheFile { clients }
}

fn write_client_cache(data_dir: &Path, next: &ClientCacheFile) -> std::io::Result<()> {
    let file = clients_file(data_dir);
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = file.with_extension(format!("{}.tmp", random_hex(4)));
    let content = serde_json::to_string_pretty(next).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, &file)?;
    Ok(())
}

fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

// hex crate may not be dep; implement simple hex
mod hex {
    pub fn encode(bytes: Vec<u8>) -> String {
        const LUT: &[u8] = b"0123456789abcdef";
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            out.push(LUT[(b >> 4) as usize] as char);
            out.push(LUT[(b & 0x0f) as usize] as char);
        }
        out
    }
}

pub async fn register_client(
    registration_endpoint: &str,
    redirect_uri: &str,
    client: &reqwest::Client,
) -> Result<(String, Option<String>), String> {
    let body = serde_json::json!({
        "redirect_uris": [redirect_uri],
        "token_endpoint_auth_method": "none",
        "grant_types": ["authorization_code","refresh_token"],
        "response_types": ["code"],
        "client_name": "OpenDesign",
        "application_type": "web",
    });
    let res = client
        .post(registration_endpoint)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        let status = res.status();
        let txt = res.text().await.unwrap_or_default();
        let txt = &txt[..txt.len().min(500)];
        return Err(format!(
            "dynamic client registration failed: HTTP {} {} {}",
            status.as_u16(),
            status.canonical_reason().unwrap_or(""),
            txt
        ));
    }
    let json: serde_json::Value = res.json().await.map_err(|e| e.to_string())?;
    let client_id = json
        .get("client_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "dynamic client registration response missing client_id".to_string())?
        .to_string();
    let client_secret = json
        .get("client_secret")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    Ok((client_id, client_secret))
}

pub async fn get_or_register_client(
    data_dir: &Path,
    auth_server: &AuthorizationServerMetadata,
    redirect_uri: &str,
    client: &reqwest::Client,
) -> Result<RegisteredClient, String> {
    let cache = read_client_cache(data_dir);
    if let Some(c) = cache
        .clients
        .iter()
        .find(|c| c.auth_server_issuer == auth_server.issuer.clone().unwrap_or_default() && c.redirect_uri == redirect_uri)
    {
        return Ok(c.clone());
    }
    let reg_endpoint = auth_server
        .registration_endpoint
        .as_ref()
        .ok_or_else(|| format!("auth server {} does not advertise a registration_endpoint and no client is pre-registered", auth_server.issuer.clone().unwrap_or_default()))?;
    let (cid, csecret) = register_client(reg_endpoint, redirect_uri, client).await?;
    let mut next = cache;
    let rc = RegisteredClient {
        auth_server_issuer: auth_server.issuer.clone().unwrap_or_default(),
        redirect_uri: redirect_uri.to_string(),
        client_id: cid,
        client_secret: csecret,
        registered_at: now_millis(),
    };
    next.clients.push(rc.clone());
    write_client_cache(data_dir, &next).map_err(|e| e.to_string())?;
    Ok(rc)
}

// ── buildAuthorizeUrl ──────────────────────────────────────────────────

pub struct AuthorizeUrlInput {
    pub auth_server: AuthorizationServerMetadata,
    pub client_id: String,
    pub redirect_uri: String,
    pub state: String,
    pub code_challenge: String,
    pub scope: Option<String>,
    pub resource: Option<String>,
}

pub fn build_authorize_url(input: AuthorizeUrlInput) -> String {
    let mut u = match Url::parse(&input.auth_server.authorization_endpoint) {
        Ok(u) => u,
        Err(_) => return input.auth_server.authorization_endpoint.clone(),
    };
    u.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &input.client_id)
        .append_pair("redirect_uri", &input.redirect_uri)
        .append_pair("state", &input.state)
        .append_pair("code_challenge", &input.code_challenge)
        .append_pair("code_challenge_method", "S256");
    if let Some(scope) = &input.scope {
        u.query_pairs_mut().append_pair("scope", scope);
    }
    if let Some(resource) = &input.resource {
        u.query_pairs_mut().append_pair("resource", resource);
    }
    u.to_string()
}

// ── Token endpoint helpers ─────────────────────────────────────────────

#[derive(Debug)]
pub struct ExchangeCodeInput {
    pub token_endpoint: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub redirect_uri: String,
    pub code: String,
    pub code_verifier: String,
    pub resource: Option<String>,
}

pub async fn exchange_code_for_token(
    input: ExchangeCodeInput,
    client: &reqwest::Client,
) -> Result<OAuthTokenResponse, String> {
    let mut form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", input.code),
        ("redirect_uri", input.redirect_uri),
        ("client_id", input.client_id.clone()),
        ("code_verifier", input.code_verifier),
    ];
    if let Some(r) = &input.resource {
        form.push(("resource", r.clone()));
    }
    token_request(&input.token_endpoint, &form, input.client_secret.as_deref(), client, input.client_id.as_str()).await
}

#[derive(Debug)]
pub struct RefreshTokenInput {
    pub token_endpoint: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub refresh_token: String,
    pub scope: Option<String>,
    pub resource: Option<String>,
}

pub async fn refresh_access_token(
    input: RefreshTokenInput,
    client: &reqwest::Client,
) -> Result<OAuthTokenResponse, String> {
    let mut form = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", input.refresh_token),
        ("client_id", input.client_id.clone()),
    ];
    if let Some(s) = &input.scope {
        form.push(("scope", s.clone()));
    }
    if let Some(r) = &input.resource {
        form.push(("resource", r.clone()));
    }
    token_request(&input.token_endpoint, &form, input.client_secret.as_deref(), client, input.client_id.as_str()).await
}

async fn token_request(
    token_endpoint: &str,
    form: &[(impl AsRef<str>, String)],
    client_secret: Option<&str>,
    client: &reqwest::Client,
    client_id: &str,
) -> Result<OAuthTokenResponse, String> {
    // build form body manually
    let mut params: Vec<(String, String)> = Vec::new();
    for (k, v) in form {
        params.push((k.as_ref().to_string(), v.clone()));
    }
    let mut req = client
        .post(token_endpoint)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json");
    if let Some(secret) = client_secret {
        use base64::engine::general_purpose::STANDARD;
        let basic = STANDARD.encode(format!("{}:{}", client_id, secret));
        req = req.header("authorization", format!("Basic {}", basic));
    }
    // encode body
    let body: String = params
        .iter()
        .map(|(k, v)| format!("{}={}", urlencoding(k), urlencoding(v)))
        .collect::<Vec<_>>()
        .join("&");
    let res = req.body(body).send().await.map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        let status = res.status();
        let txt = res.text().await.unwrap_or_default();
        let txt = &txt[..txt.len().min(500)];
        return Err(format!(
            "token endpoint rejected request: HTTP {} {} {}",
            status.as_u16(),
            status.canonical_reason().unwrap_or(""),
            txt
        ));
    }
    let json: OAuthTokenResponse = res.json().await.map_err(|e| e.to_string())?;
    if json.access_token.is_empty() {
        return Err("token endpoint response missing access_token".to_string());
    }
    Ok(json)
}

fn urlencoding(s: &str) -> String {
    // minimal percent-encoding
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

// ── beginAuth ──────────────────────────────────────────────────────────

pub struct BeginAuthInput {
    pub server_id: String,
    pub server_url: String,
    pub redirect_uri: String,
    pub data_dir: std::path::PathBuf,
    pub scope: Option<String>,
}

pub struct BeginAuthResult {
    pub authorize_url: String,
    pub state: String,
    pub pending: PendingAuthState,
}

pub async fn begin_auth(input: BeginAuthInput) -> Result<BeginAuthResult, String> {
    let client = reqwest::Client::new();
    // protected resource
    let prm = discover_protected_resource(&input.server_url, &client).await;
    let issuer_hint = prm
        .as_ref()
        .and_then(|p| p.authorization_servers.as_ref())
        .and_then(|v| v.first())
        .cloned();
    let issuer = issuer_hint.unwrap_or_else(|| {
        Url::parse(&input.server_url)
            .map(|u| format!("{}://{}", u.scheme(), u.host_str().unwrap_or("")))
            .unwrap_or_default()
    });

    let auth_server = discover_auth_server(&issuer, &client)
        .await
        .ok_or_else(|| format!("could not discover OAuth metadata for {}", issuer))?;

    let redirect_uri = input.redirect_uri.clone();
    let rc = get_or_register_client(&input.data_dir, &auth_server, &redirect_uri, &client).await?;

    let code_verifier = generate_code_verifier();
    let code_challenge = derive_code_challenge(&code_verifier);
    let state = generate_state();

    let scope = input.scope.clone().or_else(|| {
        prm.as_ref()
            .and_then(|p| p.scopes_supported.as_ref())
            .filter(|v| !v.is_empty())
            .map(|v| v.join(" "))
            .or_else(|| auth_server.scopes_supported.as_ref().map(|v| v.join(" ")))
    });
    let resource = prm
        .as_ref()
        .and_then(|p| p.resource.clone())
        .unwrap_or_else(|| input.server_url.clone());

    let auth_url_input = AuthorizeUrlInput {
        auth_server: auth_server.clone(),
        client_id: rc.client_id.clone(),
        redirect_uri: redirect_uri.clone(),
        state: state.clone(),
        code_challenge,
        scope: scope.clone(),
        resource: Some(resource.clone()),
    };
    let authorize_url = build_authorize_url(auth_url_input);

    let pending = PendingAuthState {
        server_id: input.server_id,
        auth_server_issuer: auth_server.issuer.clone().unwrap_or(issuer),
        token_endpoint: auth_server.token_endpoint,
        client_id: rc.client_id,
        client_secret: rc.client_secret,
        redirect_uri,
        code_verifier,
        scope: scope.clone(),
        resource_url: Some(resource),
        created_at: now_millis(),
    };
    // keep client secret passed above

    Ok(BeginAuthResult {
        authorize_url,
        state,
        pending,
    })
}

// Suppress unused warnings for RefreshTokenInput maybe used later
#[allow(dead_code)]
pub fn _refresh_input() -> Option<RefreshTokenInput> {
    None
}
// ── public helpers for handlers ────────────────────────────────────────

pub fn now_millis_pub() -> u64 {
    now_millis()
}

pub fn get_public_base_url(headers: &axum::http::HeaderMap) -> String {
    if let Ok(env) = std::env::var("OD_PUBLIC_BASE_URL") {
        if env.starts_with("http://") || env.starts_with("https://") {
            return env.trim_end_matches('/').to_string();
        }
    }
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");
    if let Some(host) = headers.get("host").and_then(|v| v.to_str().ok()) {
        return format!("{}://{}", proto, host);
    }
    let port = std::env::var("OD_PORT").unwrap_or_else(|_| "7456".to_string());
    format!("http://localhost:{}", port)
}

pub fn render_oauth_result_page(ok: bool, server_id: Option<&str>, message: Option<&str>) -> String {
    let title = if ok { "Connected" } else { "Authorization failed" };
    let heading = if ok { "✅ Connected" } else { "⚠️ Authorization failed" };
    let body = if ok {
        format!(
            "Your MCP server <code>{}</code> is now connected. You can close this tab and return to OpenDesign.",
            escape_html(server_id.unwrap_or(""))
        )
    } else {
        escape_html(message.unwrap_or("Authorization could not be completed."))
    };
    let accent = if ok { "#1a7f37" } else { "#cf222e" };
    let payload = if ok {
        serde_json::json!({"type":"mcp-oauth","ok":true,"serverId":server_id})
    } else {
        serde_json::json!({"type":"mcp-oauth","ok":false,"message":message})
    };
    let payload_str = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<title>{title} — OpenDesign</title>
<meta name="viewport" content="width=device-width, initial-scale=1" />
<style>
  :root {{ color-scheme: light dark; }}
  html, body {{ height: 100%; margin: 0; }}
  body {{
    display: flex; align-items: center; justify-content: center;
    font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, Helvetica, sans-serif;
    background: #f6f7f9; color: #1f2328; padding: 24px;
  }}
  .card {{ max-width: 420px; width: 100%; padding: 28px 28px 22px; border-radius: 12px; background: white; border: 1px solid #d0d7de; box-shadow: 0 8px 24px rgba(0,0,0,.06); text-align: left; }}
  h1 {{ margin: 0 0 8px; font-size: 18px; color: {accent}; }}
  p {{ margin: 0 0 16px; font-size: 14px; line-height: 1.55; }}
  code {{ background: #f3f4f6; padding: 1px 6px; border-radius: 4px; font-size: 12.5px; }}
  button {{ appearance: none; border: 1px solid #d0d7de; background: white; border-radius: 8px; padding: 8px 14px; font-size: 13px; cursor: pointer; }}
</style>
</head>
<body>
  <div class="card">
    <h1>{heading}</h1>
    <p>{body}</p>
    <button type="button" onclick="window.close()">Close this tab</button>
  </div>
  <script>
    try {{
      var payload = {payload};
      if (window.opener && !window.opener.closed) {{ window.opener.postMessage(payload, '*'); }}
      if (window.BroadcastChannel) {{ var bc = new BroadcastChannel('open-design-mcp-oauth'); bc.postMessage(payload); bc.close(); }}
    }} catch (e) {{ }}
  </script>
</body>
</html>"#,
        title = escape_html(title),
        accent = accent,
        heading = escape_html(heading),
        body = body,
        payload = payload_str,
    )
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
