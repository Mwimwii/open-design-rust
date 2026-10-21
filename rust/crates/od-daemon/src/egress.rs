//! Provider-egress guards — Rust parity for the base-URL and reasoning-egress
//! policy shared by the BYOK proxy routes.
//!
//! Sources:
//! * `packages/contracts/src/api/connectionTest.ts` — `validateBaseUrl`,
//!   `isLoopbackApiHost`, `isBlockedExternalApiHostname`,
//!   `isAllowlistedInternalHost`.
//! * `apps/daemon/src/connectionTest.ts` — `validateBaseUrlResolved` (DNS-aware
//!   companion) and `validateUserProviderBaseUrl`.
//! * `apps/daemon/src/origin-validation.ts` — `configuredAllowedInternalHosts`.
//! * `apps/daemon/src/reasoning-egress.ts` — `authorizeReasoningEgress` and the
//!   denial envelope.
//!
//! The split matters: user-configured provider endpoints keep the loopback
//! carve-out (local gateways such as Ollama), while attacker-controllable
//! asset URLs use [`ValidateBaseUrlOptions::forbid_loopback`] and never
//! consult the operator allowlist.

use std::net::IpAddr;

use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};
use url::Url;

/// Hosts the operator has explicitly declared trusted (`OD_ALLOWED_INTERNAL_HOSTS`).
#[derive(Debug, Clone, Default)]
pub struct ValidateBaseUrlOptions {
    pub allowed_internal_hosts: Vec<String>,
    /// Asset-download URLs (attacker-controllable) must never reach loopback.
    pub forbid_loopback: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsAddress {
    pub address: String,
    pub family: u8,
}

#[derive(Debug, Clone, Default)]
pub struct BaseUrlValidation {
    pub parsed: Option<Url>,
    pub error: Option<String>,
    pub forbidden: bool,
    /// Addresses resolved by DNS that passed validation, for callers that pin
    /// the connection to them (DNS-rebinding defence, TS issue #5478).
    pub resolved_addresses: Vec<DnsAddress>,
}

fn rejected(error: &str) -> BaseUrlValidation {
    BaseUrlValidation {
        error: Some(error.to_string()),
        ..Default::default()
    }
}

fn forbidden(error: &str) -> BaseUrlValidation {
    BaseUrlValidation {
        error: Some(error.to_string()),
        forbidden: true,
        ..Default::default()
    }
}

fn normalize_bracketed_ipv6(hostname: &str) -> String {
    let stripped = hostname
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(hostname);
    stripped
        .to_lowercase()
        .trim_end_matches('.')
        .to_string()
}

fn parse_ipv4(hostname: &str) -> Option<[u8; 4]> {
    let parts: Vec<&str> = hostname.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut out = [0u8; 4];
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty()
            || part.len() > 3
            || !part.chars().all(|c| c.is_ascii_digit())
        {
            return None;
        }
        let value: u32 = part.parse().ok()?;
        if value > 255 {
            return None;
        }
        out[index] = value as u8;
    }
    Some(out)
}

fn is_loopback_ipv4(hostname: &str) -> bool {
    parse_ipv4(hostname).map(|parts| parts[0] == 127).unwrap_or(false)
}

fn is_blocked_ipv4(hostname: &str) -> bool {
    let Some(parts) = parse_ipv4(hostname) else {
        return false;
    };
    let (a, b) = (parts[0], parts[1]);
    a == 0
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || a == 10
        || (a == 192 && b == 168)
        || (a == 172 && (16..=31).contains(&b))
        || a >= 224
}

fn ipv4_mapped_to_dotted(hostname: &str) -> Option<String> {
    let host = normalize_bracketed_ipv6(hostname);
    let mapped = host.strip_prefix("::ffff:")?;
    if parse_ipv4(mapped).is_some() {
        return Some(mapped.to_string());
    }
    let hex_parts: Vec<&str> = mapped.split(':').collect();
    if hex_parts.len() != 2 {
        return None;
    }
    if !hex_parts
        .iter()
        .all(|part| (1..=4).contains(&part.len()) && part.chars().all(|c| c.is_ascii_hexdigit()))
    {
        return None;
    }
    let hi = u32::from_str_radix(hex_parts[0], 16).ok()?;
    let lo = u32::from_str_radix(hex_parts[1], 16).ok()?;
    let value = (hi << 16) | lo;
    Some(format!(
        "{}.{}.{}.{}",
        (value >> 24) & 255,
        (value >> 16) & 255,
        (value >> 8) & 255,
        value & 255
    ))
}

/// `localhost`, `::1`, `127.0.0.0/8` (including IPv4-mapped IPv6 forms).
pub fn is_loopback_api_host(hostname: &str) -> bool {
    let host = normalize_bracketed_ipv6(hostname);
    if host == "localhost" || host == "::1" {
        return true;
    }
    if is_loopback_ipv4(&host) {
        return true;
    }
    ipv4_mapped_to_dotted(&host)
        .map(|mapped| is_loopback_ipv4(&mapped))
        .unwrap_or(false)
}

/// Private, link-local, CGNAT, metadata, multicast and unspecified addresses.
pub fn is_blocked_external_api_hostname(hostname: &str) -> bool {
    let host = normalize_bracketed_ipv6(hostname);
    if host == "::" {
        return true;
    }
    if is_blocked_ipv4(&host) {
        return true;
    }
    if is_unique_local_ipv6(&host) || is_link_local_ipv6(&host) {
        return true;
    }
    ipv4_mapped_to_dotted(&host)
        .map(|mapped| is_blocked_ipv4(&mapped))
        .unwrap_or(false)
}

fn is_unique_local_ipv6(host: &str) -> bool {
    // fc00::/7 — first byte 0xfc or 0xfd.
    let Some(rest) = host.strip_prefix("fc").or_else(|| host.strip_prefix("fd")) else {
        return false;
    };
    rest.len() >= 3
        && rest.chars().take(2).all(|c| c.is_ascii_hexdigit())
        && rest.as_bytes().get(2) == Some(&b':')
}

fn is_link_local_ipv6(host: &str) -> bool {
    // fe80::/10 — `fe8`, `fe9`, `fea`, `feb`.
    let Some(rest) = host.strip_prefix("fe") else {
        return false;
    };
    let mut chars = rest.chars();
    matches!(chars.next(), Some('8' | '9' | 'a' | 'b'))
        && chars.next().map(|c| c.is_ascii_hexdigit()).unwrap_or(false)
        && rest.as_bytes().get(2) == Some(&b':')
}

fn internal_host_match_forms(hostname: &str) -> Vec<String> {
    let mut forms = vec![normalize_bracketed_ipv6(hostname)];
    if let Some(mapped) = ipv4_mapped_to_dotted(hostname) {
        forms.push(mapped.to_lowercase());
    }
    forms
}

/// `OD_ALLOWED_INTERNAL_HOSTS` escape hatch (TS issue #3225). An empty list
/// always returns false, so the default stays default-deny.
pub fn is_allowlisted_internal_host(hostname: &str, allowed_internal_hosts: &[String]) -> bool {
    if allowed_internal_hosts.is_empty() {
        return false;
    }
    let candidate_forms = internal_host_match_forms(hostname);
    allowed_internal_hosts.iter().any(|entry| {
        if entry.trim().is_empty() {
            return false;
        }
        internal_host_match_forms(entry.trim())
            .iter()
            .any(|form| candidate_forms.contains(form))
    })
}

fn looks_like_ip_literal(hostname: &str) -> bool {
    let host = hostname
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(hostname);
    parse_ipv4(host).is_some() || host.contains(':')
}

/// Literal-string half of the guard (no DNS).
pub fn validate_base_url(base_url: &str, options: &ValidateBaseUrlOptions) -> BaseUrlValidation {
    let trimmed = base_url.trim_end_matches('/');
    let parsed = match Url::parse(trimmed) {
        Ok(parsed) => parsed,
        Err(_) => return rejected("Invalid baseUrl"),
    };
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return rejected("Only http/https allowed");
    }
    let hostname = parsed.host_str().unwrap_or_default().to_lowercase();
    if options.forbid_loopback && is_loopback_api_host(&hostname) {
        return forbidden("Loopback addresses blocked for asset URLs");
    }
    if !is_loopback_api_host(&hostname)
        && !is_allowlisted_internal_host(&hostname, &options.allowed_internal_hosts)
        && is_blocked_external_api_hostname(&hostname)
    {
        return forbidden("Internal IPs blocked");
    }
    BaseUrlValidation {
        parsed: Some(parsed),
        ..Default::default()
    }
}

/// DNS-aware companion: a public hostname that resolves to an internal address
/// is rejected before the upstream request is issued.
pub async fn validate_base_url_resolved(
    base_url: &str,
    options: &ValidateBaseUrlOptions,
) -> BaseUrlValidation {
    let sync = validate_base_url(base_url, options);
    let Some(parsed) = sync.parsed.clone() else {
        return sync;
    };
    let hostname = parsed.host_str().unwrap_or_default().to_lowercase();
    if !options.forbid_loopback && is_loopback_api_host(&hostname) {
        return sync;
    }
    if is_allowlisted_internal_host(&hostname, &options.allowed_internal_hosts) {
        return sync;
    }
    if looks_like_ip_literal(&hostname) {
        return sync;
    }
    let resolved = resolve_host(&hostname).await;
    finish_validation(sync, options, resolved)
}

async fn resolve_host(hostname: &str) -> Result<Vec<IpAddr>, ()> {
    let lookup = tokio::net::lookup_host((hostname, 0)).await;
    match lookup {
        Ok(addresses) => Ok(addresses.map(|address| address.ip()).collect()),
        Err(_) => Err(()),
    }
}

/// The post-DNS half, split out so tests can drive it with a fixed address
/// list instead of depending on the resolver.
pub(crate) fn finish_validation(
    mut validation: BaseUrlValidation,
    options: &ValidateBaseUrlOptions,
    resolved: Result<Vec<IpAddr>, ()>,
) -> BaseUrlValidation {
    let addresses = match resolved {
        Ok(addresses) => addresses,
        Err(()) => {
            // Asset URLs fail closed on a resolver hiccup; user-configured
            // provider endpoints surface the connection error from `fetch`.
            if options.forbid_loopback {
                return forbidden("DNS resolution failed for asset URL");
            }
            return validation;
        }
    };
    let mut kept = Vec::with_capacity(addresses.len());
    for address in &addresses {
        let ip = address.to_string().to_lowercase();
        if is_loopback_api_host(&ip) {
            if options.forbid_loopback {
                return forbidden(&format!("DNS-resolved loopback address blocked ({ip})"));
            }
            continue;
        }
        if is_allowlisted_internal_host(&ip, &options.allowed_internal_hosts) {
            continue;
        }
        if is_blocked_external_api_hostname(&ip) {
            return forbidden("Internal IPs blocked");
        }
        kept.push(DnsAddress {
            address: ip,
            family: if address.is_ipv4() { 4 } else { 6 },
        });
    }
    validation.resolved_addresses = kept;
    validation
}

/// User-configured provider endpoint (connection test, model discovery, BYOK
/// chat dispatch). Honors the operator allowlist.
pub async fn validate_user_provider_base_url(base_url: &str) -> BaseUrlValidation {
    validate_base_url_resolved(
        base_url,
        &ValidateBaseUrlOptions {
            allowed_internal_hosts: configured_allowed_internal_hosts(),
            forbid_loopback: false,
        },
    )
    .await
}

pub fn configured_allowed_internal_hosts() -> Vec<String> {
    configured_allowed_internal_hosts_from(std::env::var("OD_ALLOWED_INTERNAL_HOSTS").ok().as_deref())
}

pub fn configured_allowed_internal_hosts_from(raw: Option<&str>) -> Vec<String> {
    let raw = raw.unwrap_or_default();
    if raw.trim().is_empty() {
        return Vec::new();
    }
    let mut hosts = Vec::new();
    for part in raw.split(|c: char| c == ',' || c.is_whitespace()) {
        let entry = part.trim();
        if entry.is_empty() {
            continue;
        }
        if !entry.contains("://") && is_cidr_like(entry) {
            tracing::warn!(
                entry = %entry,
                "[ssrf] ignoring CIDR entry in OD_ALLOWED_INTERNAL_HOSTS — list individual hosts instead"
            );
            continue;
        }
        let with_scheme = if entry.contains("://") {
            entry.to_string()
        } else {
            format!("http://{entry}")
        };
        let hostname = match Url::parse(&with_scheme) {
            Ok(url) => url
                .host_str()
                .map(|host| host.trim_matches(|c| c == '[' || c == ']').to_lowercase())
                .unwrap_or_default(),
            Err(_) => String::new(),
        };
        if hostname.is_empty() {
            tracing::warn!(
                entry = %entry,
                "[ssrf] ignoring malformed OD_ALLOWED_INTERNAL_HOSTS entry"
            );
            continue;
        }
        hosts.push(hostname);
    }
    hosts
}

fn is_cidr_like(entry: &str) -> bool {
    let Some((host, prefix)) = entry.split_once('/') else {
        return false;
    };
    !host.is_empty()
        && !host.contains('/')
        && (1..=3).contains(&prefix.len())
        && prefix.chars().all(|c| c.is_ascii_digit())
}

// ---------------------------------------------------------------------------
// Reasoning egress policy
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningEgressRouteKind {
    Proxy,
    ProviderModels,
    ConnectionTest,
    Finalize,
}

impl ReasoningEgressRouteKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proxy => "proxy",
            Self::ProviderModels => "provider_models",
            Self::ConnectionTest => "connection_test",
            Self::Finalize => "finalize",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReasoningEgressRequest {
    /// `reasoningExecution` from the request body. `None` means "absent",
    /// which the policy treats as `enabled`; `Some(Value::Null)` is invalid.
    pub policy: Option<Value>,
    pub route_kind: ReasoningEgressRouteKind,
    pub provider: String,
    pub resolved_base_url: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasoningEgressDenial {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
    pub data: Value,
}

impl ReasoningEgressDenial {
    pub fn into_response(self) -> Response {
        let status = axum::http::StatusCode::from_u16(self.status)
            .unwrap_or(axum::http::StatusCode::FORBIDDEN);
        (
            status,
            Json(json!({
                "error": {
                    "code": self.code,
                    "message": self.message,
                    "data": self.data,
                }
            })),
        )
            .into_response()
    }
}

fn policy_object(policy: &Option<Value>) -> Option<&Map<String, Value>> {
    match policy {
        Some(Value::Object(map)) => Some(map),
        _ => None,
    }
}

fn policy_mode(policy: &Option<Value>) -> &'static str {
    match policy {
        None => "enabled",
        Some(Value::Object(map)) => match map.get("mode").and_then(Value::as_str) {
            Some("enabled") => "enabled",
            Some("disabled") => "disabled",
            Some("allowlist") => "allowlist",
            _ => "invalid",
        },
        Some(_) => "invalid",
    }
}

fn denial_data(request: &ReasoningEgressRequest, policy_mode: Option<&str>) -> Value {
    let mut data = Map::new();
    data.insert("routeKind".to_string(), json!(request.route_kind.as_str()));
    data.insert("provider".to_string(), json!(request.provider));
    if let Some(mode) = policy_mode {
        data.insert("policyMode".to_string(), json!(mode));
    }
    if let Some(base_url) = request.resolved_base_url.as_deref().filter(|v| !v.is_empty()) {
        data.insert("resolvedBaseUrl".to_string(), json!(base_url));
    }
    if let Some(model) = request.model.as_deref().filter(|v| !v.is_empty()) {
        data.insert("model".to_string(), json!(model));
    }
    Value::Object(data)
}

fn invalid_policy_denial(request: &ReasoningEgressRequest) -> ReasoningEgressDenial {
    ReasoningEgressDenial {
        status: 400,
        code: "reasoning_execution_invalid_policy",
        message: "reasoningExecution.mode must be one of enabled, disabled, or allowlist."
            .to_string(),
        data: denial_data(request, None),
    }
}

fn policy_denial(
    request: &ReasoningEgressRequest,
    mode: &'static str,
    code: &'static str,
    message: &str,
) -> ReasoningEgressDenial {
    ReasoningEgressDenial {
        status: 403,
        code,
        message: message.to_string(),
        data: denial_data(request, Some(mode)),
    }
}

fn disabled_denial(request: &ReasoningEgressRequest) -> ReasoningEgressDenial {
    policy_denial(
        request,
        "disabled",
        "reasoning_execution_disabled",
        "Reasoning provider egress is disabled for this run.",
    )
}

fn allowlist_denial(request: &ReasoningEgressRequest) -> ReasoningEgressDenial {
    policy_denial(
        request,
        "allowlist",
        "reasoning_execution_not_allowlisted",
        "Reasoning provider egress is not allowlisted for this run.",
    )
}

fn route_is_denied_by_flag(policy: &Map<String, Value>, route: ReasoningEgressRouteKind) -> bool {
    let flag = match route {
        ReasoningEgressRouteKind::ProviderModels => "denyProviderDiscovery",
        ReasoningEgressRouteKind::ConnectionTest => "denyConnectionTests",
        ReasoningEgressRouteKind::Finalize => "denyFinalize",
        ReasoningEgressRouteKind::Proxy => return false,
    };
    policy.get(flag).and_then(Value::as_bool).unwrap_or(false)
}

/// `new URL(value)` with hash/search dropped and trailing slashes removed.
pub fn normalize_reasoning_base_url(value: &str) -> Option<String> {
    let mut url = Url::parse(value).ok()?;
    url.set_fragment(None);
    url.set_query(None);
    let path = url.path().trim_end_matches('/').to_string();
    url.set_path(if path.is_empty() { "/" } else { path.as_str() });
    Some(url.to_string().trim_end_matches('/').to_string())
}

fn normalize_reasoning_model_id(provider: &str, model: &str) -> String {
    let trimmed = model.trim();
    if provider == "google" {
        return trimmed
            .strip_prefix("models/")
            .unwrap_or(trimmed)
            .to_string();
    }
    trimmed.to_string()
}

fn allowed_base_url_set(policy: &Map<String, Value>) -> Vec<String> {
    policy
        .get("allowedBaseUrls")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .filter_map(normalize_reasoning_base_url)
                .collect()
        })
        .unwrap_or_default()
}

fn allowed_model_set(policy: &Map<String, Value>, provider: &str) -> Vec<String> {
    policy
        .get("allowedModels")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(|value| normalize_reasoning_model_id(provider, value))
                .filter(|value| !value.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Returns the denial a route must answer with, or `None` when egress is
/// allowed. Fails closed: an unrecognised policy shape is a 400, never an
/// implicit allow.
pub fn authorize_reasoning_egress(
    request: &ReasoningEgressRequest,
) -> Option<ReasoningEgressDenial> {
    match policy_mode(&request.policy) {
        "enabled" => return None,
        "invalid" => return Some(invalid_policy_denial(request)),
        "disabled" => return Some(disabled_denial(request)),
        _ => {}
    }

    let policy = policy_object(&request.policy)?;
    if route_is_denied_by_flag(policy, request.route_kind) {
        return Some(allowlist_denial(request));
    }

    let normalized_base_url = request
        .resolved_base_url
        .as_deref()
        .and_then(normalize_reasoning_base_url);
    match normalized_base_url {
        Some(base_url) if allowed_base_url_set(policy).contains(&base_url) => {}
        _ => return Some(allowlist_denial(request)),
    }

    if let Some(model) = request.model.as_deref() {
        if !allowed_model_set(policy, &request.provider)
            .contains(&normalize_reasoning_model_id(&request.provider, model))
        {
            return Some(allowlist_denial(request));
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> ValidateBaseUrlOptions {
        ValidateBaseUrlOptions::default()
    }

    #[test]
    fn literal_host_blocks_match_the_contracts_guard() {
        for host in [
            "http://10.0.0.5/v1",
            "http://192.168.1.1",
            "http://172.16.0.1",
            "http://169.254.169.254/latest/meta-data",
            "http://100.64.0.1",
            "http://0.0.0.0",
            "http://[::]",
            "http://[fc00::1]",
            "http://[fe80::1]",
            "http://[::ffff:10.0.0.5]",
        ] {
            let result = validate_base_url(host, &options());
            assert!(result.forbidden, "{host} must be forbidden");
            assert_eq!(result.error.as_deref(), Some("Internal IPs blocked"));
        }
    }

    #[test]
    fn loopback_is_allowed_for_provider_endpoints() {
        for host in ["http://127.0.0.1:11434", "http://localhost:11434", "http://[::1]:8080"] {
            let result = validate_base_url(host, &options());
            assert!(result.parsed.is_some(), "{host} must parse: {:?}", result.error);
            assert!(!result.forbidden);
        }
    }

    #[test]
    fn forbid_loopback_rejects_loopback_assets() {
        let opts = ValidateBaseUrlOptions {
            forbid_loopback: true,
            ..Default::default()
        };
        let result = validate_base_url("http://127.0.0.1/asset.png", &opts);
        assert!(result.forbidden);
        assert_eq!(
            result.error.as_deref(),
            Some("Loopback addresses blocked for asset URLs")
        );
    }

    #[test]
    fn operator_allowlist_exempts_internal_hosts() {
        let opts = ValidateBaseUrlOptions {
            allowed_internal_hosts: vec!["10.0.0.5".to_string()],
            ..Default::default()
        };
        assert!(validate_base_url("http://10.0.0.5/v1", &opts).parsed.is_some());
        assert!(validate_base_url("http://10.0.0.6/v1", &opts).forbidden);
    }

    #[test]
    fn scheme_and_shape_errors_use_the_ts_wording() {
        assert_eq!(
            validate_base_url("ftp://example.com", &options()).error.as_deref(),
            Some("Only http/https allowed")
        );
        assert_eq!(
            validate_base_url("not a url", &options()).error.as_deref(),
            Some("Invalid baseUrl")
        );
    }

    #[test]
    fn dns_resolution_blocks_private_addresses() {
        let sync = validate_base_url("https://internal.example.com/v1", &options());
        let blocked = finish_validation(
            sync.clone(),
            &options(),
            Ok(vec!["10.1.2.3".parse().unwrap()]),
        );
        assert!(blocked.forbidden);
        assert_eq!(blocked.error.as_deref(), Some("Internal IPs blocked"));

        let public = finish_validation(
            sync.clone(),
            &options(),
            Ok(vec!["93.184.216.34".parse().unwrap()]),
        );
        assert!(!public.forbidden);
        assert_eq!(
            public.resolved_addresses,
            vec![DnsAddress {
                address: "93.184.216.34".to_string(),
                family: 4
            }]
        );
    }

    #[test]
    fn dns_loopback_skips_for_providers_and_fails_closed_for_assets() {
        let sync = validate_base_url("https://internal.example.com/v1", &options());
        let skipped = finish_validation(
            sync.clone(),
            &options(),
            Ok(vec!["127.0.0.1".parse().unwrap()]),
        );
        assert!(!skipped.forbidden);
        assert!(skipped.resolved_addresses.is_empty());

        let strict = ValidateBaseUrlOptions {
            forbid_loopback: true,
            ..Default::default()
        };
        let blocked = finish_validation(
            sync.clone(),
            &strict,
            Ok(vec!["127.0.0.1".parse().unwrap()]),
        );
        assert!(blocked.forbidden);
        assert_eq!(
            blocked.error.as_deref(),
            Some("DNS-resolved loopback address blocked (127.0.0.1)")
        );

        let failed = finish_validation(sync, &strict, Err(()));
        assert!(failed.forbidden);
        assert_eq!(
            failed.error.as_deref(),
            Some("DNS resolution failed for asset URL")
        );
    }

    #[test]
    fn dns_failure_is_not_a_security_signal_for_providers() {
        let sync = validate_base_url("https://provider.example.com/v1", &options());
        let result = finish_validation(sync, &options(), Err(()));
        assert!(!result.forbidden);
        assert!(result.error.is_none());
    }

    #[test]
    fn configured_allowlist_parses_hosts_and_rejects_cidrs() {
        assert_eq!(
            configured_allowed_internal_hosts_from(Some("10.0.0.5, https://Gateway.Example.com:8443 [::1]")),
            vec![
                "10.0.0.5".to_string(),
                "gateway.example.com".to_string(),
                "::1".to_string()
            ]
        );
        assert!(configured_allowed_internal_hosts_from(Some("10.0.0.0/24")).is_empty());
        assert!(configured_allowed_internal_hosts_from(None).is_empty());
    }

    fn request(policy: Option<Value>) -> ReasoningEgressRequest {
        ReasoningEgressRequest {
            policy,
            route_kind: ReasoningEgressRouteKind::Proxy,
            provider: "openai".to_string(),
            resolved_base_url: Some("https://api.openai.com/v1".to_string()),
            model: Some("gpt-test".to_string()),
        }
    }

    #[test]
    fn absent_policy_allows_egress() {
        assert!(authorize_reasoning_egress(&request(None)).is_none());
    }

    #[test]
    fn malformed_policy_is_a_400() {
        for policy in [json!({}), json!({"mode": "off"}), Value::Null, json!([])] {
            let denial = authorize_reasoning_egress(&request(Some(policy))).expect("denied");
            assert_eq!(denial.status, 400);
            assert_eq!(denial.code, "reasoning_execution_invalid_policy");
            assert_eq!(denial.data["routeKind"], json!("proxy"));
            assert_eq!(denial.data["provider"], json!("openai"));
        }
    }

    #[test]
    fn disabled_policy_is_a_403_with_the_ts_message() {
        let denial =
            authorize_reasoning_egress(&request(Some(json!({"mode": "disabled"})))).expect("denied");
        assert_eq!(denial.status, 403);
        assert_eq!(denial.code, "reasoning_execution_disabled");
        assert_eq!(denial.message, "Reasoning provider egress is disabled for this run.");
        assert_eq!(denial.data["policyMode"], json!("disabled"));
        assert_eq!(denial.data["resolvedBaseUrl"], json!("https://api.openai.com/v1"));
        assert_eq!(denial.data["model"], json!("gpt-test"));
    }

    #[test]
    fn allowlist_requires_base_url_and_model() {
        let allowed = json!({
            "mode": "allowlist",
            "allowedBaseUrls": ["https://api.openai.com/v1/"],
            "allowedModels": ["gpt-test"]
        });
        assert!(authorize_reasoning_egress(&request(Some(allowed))).is_none());

        let wrong_base = json!({
            "mode": "allowlist",
            "allowedBaseUrls": ["https://api.anthropic.com"],
            "allowedModels": ["gpt-test"]
        });
        let denial = authorize_reasoning_egress(&request(Some(wrong_base))).expect("denied");
        assert_eq!(denial.code, "reasoning_execution_not_allowlisted");

        let wrong_model = json!({
            "mode": "allowlist",
            "allowedBaseUrls": ["https://api.openai.com/v1"],
            "allowedModels": ["other-model"]
        });
        let denial = authorize_reasoning_egress(&request(Some(wrong_model))).expect("denied");
        assert_eq!(denial.code, "reasoning_execution_not_allowlisted");
    }

    #[test]
    fn allowlist_route_flags_deny_discovery_tests_and_finalize() {
        for (route, flag) in [
            (ReasoningEgressRouteKind::ProviderModels, "denyProviderDiscovery"),
            (ReasoningEgressRouteKind::ConnectionTest, "denyConnectionTests"),
            (ReasoningEgressRouteKind::Finalize, "denyFinalize"),
        ] {
            let mut policy = Map::new();
            policy.insert("mode".to_string(), json!("allowlist"));
            policy.insert(
                "allowedBaseUrls".to_string(),
                json!(["https://api.openai.com/v1"]),
            );
            policy.insert("allowedModels".to_string(), json!(["gpt-test"]));
            policy.insert(flag.to_string(), json!(true));
            let mut req = request(Some(Value::Object(policy)));
            req.route_kind = route;
            let denial = authorize_reasoning_egress(&req).expect("denied");
            assert_eq!(denial.code, "reasoning_execution_not_allowlisted");
            assert_eq!(denial.data["routeKind"], json!(route.as_str()));
        }
    }

    #[test]
    fn google_models_prefix_is_normalized_in_the_allowlist() {
        let mut req = request(Some(json!({
            "mode": "allowlist",
            "allowedBaseUrls": ["https://generativelanguage.googleapis.com"],
            "allowedModels": ["gemini-2.5-pro"]
        })));
        req.provider = "google".to_string();
        req.resolved_base_url = Some("https://generativelanguage.googleapis.com/".to_string());
        req.model = Some("models/gemini-2.5-pro".to_string());
        assert!(authorize_reasoning_egress(&req).is_none());
    }

    #[test]
    fn base_url_normalization_drops_query_hash_and_trailing_slashes() {
        assert_eq!(
            normalize_reasoning_base_url("https://api.openai.com/v1/?a=1#frag").as_deref(),
            Some("https://api.openai.com/v1")
        );
        assert_eq!(
            normalize_reasoning_base_url("https://api.openai.com/").as_deref(),
            Some("https://api.openai.com")
        );
    }
}
