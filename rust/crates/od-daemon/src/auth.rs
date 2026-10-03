//! API token authentication — parity port of `apps/daemon/src/api-token-auth.ts`
//! plus the `server.ts` probe-path / loopback rules.
//!
//! The decision function is pure so the security rules are unit-testable
//! without a socket.

use std::net::SocketAddr;

use base64::Engine;

/// Basic-auth username the daemon mints and accepts (parity with
/// `API_TOKEN_BASIC_USERNAME`).
pub const API_TOKEN_BASIC_USERNAME: &str = "open-design";

/// Health / readiness / version stay open even when token auth is on
/// (parity: `openProbePaths` in `server.ts`).
pub const OPEN_PROBE_PATHS: &[&str] = &[
    "/health",
    "/api/health",
    "/ready",
    "/api/ready",
    "/version",
    "/api/version",
];

/// The pieces of a request the auth rule needs.
#[derive(Debug, Clone, Copy)]
pub struct AuthRequest<'a> {
    pub path: &'a str,
    /// Transport peer. `None` means the server was not built with connect
    /// info — fail closed (never treat an unknown peer as loopback).
    pub peer: Option<SocketAddr>,
    pub authorization: Option<&'a str>,
}

/// Decide whether a request may proceed.
///
/// Order matches the TypeScript middleware:
/// 1. no active token → everything allowed (auth disabled),
/// 2. open probe paths → allowed,
/// 3. loopback peer → allowed (the desktop UI never carries credentials),
/// 4. exact `Bearer` / `Basic` token match → allowed, else denied.
pub fn authorize(request: AuthRequest<'_>, active_token: Option<&str>) -> bool {
    let Some(token) = active_token.filter(|token| !token.is_empty()) else {
        return true;
    };
    if OPEN_PROBE_PATHS.contains(&request.path) {
        return true;
    }
    if request.peer.is_some_and(|peer| peer.ip().is_loopback()) {
        return true;
    }
    match request.authorization {
        Some(value) => authorization_matches(value, token),
        None => false,
    }
}

/// `Bearer <token>` or `Basic base64(open-design:<token>)` exact match.
pub fn authorization_matches(value: &str, expected_token: &str) -> bool {
    if let Some(bearer) = scheme_value(value, "Bearer") {
        return secrets_match(bearer, expected_token);
    }
    if let Some(encoded) = scheme_value(value, "Basic") {
        let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
            return false;
        };
        let Ok(decoded) = String::from_utf8(decoded) else {
            return false;
        };
        let Some((username, password)) = decoded.split_once(':') else {
            return false;
        };
        return username == API_TOKEN_BASIC_USERNAME && secrets_match(password, expected_token);
    }
    false
}

/// Parse `"<scheme>[\t ]+<token without whitespace>[\t ]*"` case-insensitively,
/// mirroring the TypeScript `/^(Bearer|Basic)[\t ]+(\S+)[\t ]*$/i` regexes.
fn scheme_value<'a>(value: &'a str, scheme: &str) -> Option<&'a str> {
    if value.len() <= scheme.len() || !value[..scheme.len()].eq_ignore_ascii_case(scheme) {
        return None;
    }
    let rest = &value[scheme.len()..];
    let mut chars = rest.chars();
    match chars.next() {
        Some('\t') | Some(' ') => {}
        _ => return None,
    }
    let trimmed = rest.trim_matches(['\t', ' ']);
    if trimmed.is_empty() || trimmed.chars().any(char::is_whitespace) {
        return None;
    }
    Some(trimmed)
}

/// Length-checked constant-time comparison (parity: `timingSafeEqual` after an
/// explicit length check).
fn secrets_match(actual: &str, expected: &str) -> bool {
    let (actual, expected) = (actual.as_bytes(), expected.as_bytes());
    if actual.len() != expected.len() {
        return false;
    }
    actual
        .iter()
        .zip(expected)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    const TOKEN: &str = "s3cret-token";

    fn loopback() -> Option<SocketAddr> {
        Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 40000))
    }

    fn remote() -> Option<SocketAddr> {
        Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 5)), 40000))
    }

    fn req<'a>(path: &'a str, peer: Option<SocketAddr>, authorization: Option<&'a str>) -> AuthRequest<'a> {
        AuthRequest { path, peer, authorization }
    }

    #[test]
    fn disabled_when_no_token() {
        assert!(authorize(req("/api/projects", remote(), None), None));
        assert!(authorize(req("/api/projects", remote(), None), Some("")));
    }

    #[test]
    fn probe_paths_open_to_remote() {
        for path in OPEN_PROBE_PATHS {
            assert!(authorize(req(path, remote(), None), Some(TOKEN)), "{path}");
        }
    }

    #[test]
    fn loopback_skips_credentials() {
        assert!(authorize(req("/api/projects", loopback(), None), Some(TOKEN)));
    }

    #[test]
    fn remote_requires_exact_bearer() {
        let auth = format!("Bearer {TOKEN}");
        assert!(authorize(req("/api/projects", remote(), Some(&auth)), Some(TOKEN)));
        for bad in [
            "Bearer wrong".to_string(),
            format!("Bearer {TOKEN}x"),
            "Basic d3JvbmM=".to_string(),
            "bearer".to_string(),
        ] {
            assert!(
                !authorize(req("/api/projects", remote(), Some(&bad)), Some(TOKEN)),
                "should deny: {bad}"
            );
        }
    }

    #[test]
    fn basic_accepts_only_open_design_username() {
        let good = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD
                .encode(format!("{API_TOKEN_BASIC_USERNAME}:{TOKEN}"))
        );
        assert!(authorize(req("/api/projects", remote(), Some(&good)), Some(TOKEN)));

        let other_user = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("admin:{TOKEN}"))
        );
        assert!(!authorize(req("/api/projects", remote(), Some(&other_user)), Some(TOKEN)));
    }

    #[test]
    fn missing_peer_fails_closed() {
        assert!(!authorize(req("/api/projects", None, None), Some(TOKEN)));
        let auth = format!("Bearer {TOKEN}");
        assert!(authorize(req("/api/projects", None, Some(&auth)), Some(TOKEN)));
    }
}
