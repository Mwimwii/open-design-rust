//! Static-SPA fallback policy — parity port of `apps/daemon/src/static-spa.ts`.
//!
//! The daemon serves the built web export (Next.js `output: 'export'`) from
//! `web_dist` and answers unknown non-API paths with `index.html` so deep links
//! hydrate in the client router. Which requests qualify is a correctness
//! decision, not a convenience: an asset that does not exist must stay a 404
//! instead of quietly becoming a 200 HTML shell.
//!
//! DOCUMENTED DEVIATIONS
//!
//! * The TypeScript side answers the shell with Express `res.sendFile`, which
//!   carries ETag / Last-Modified / Range handling and 304 negotiation. The
//!   Rust daemon reads `index.html` and answers with plain bytes, the same
//!   stance the other file routes take (`/raw`, skill assets).
//! * `Accept` matching is a literal substring check on both sides
//!   (`accept.includes('text/html')`), so header values are compared as
//!   written rather than parsed into a media-type list.
//! * A miss that is not a shell answers the daemon's JSON error envelope
//!   (`{"error": {"code", "message"}}`) where Express answers its default HTML
//!   404 page. Status codes and the shell/no-shell decision are identical.

use axum::http::Method;

/// Path prefixes that never receive the SPA shell. Each entry matches the
/// exact path and anything beneath it, never a longer sibling (`/api` matches
/// `/api` and `/api/x`, not `/apiX`) — parity: the `req.path === p ||
/// req.path.startsWith(p + '/')` chain in `isStaticSpaFallbackRequest`.
pub const SPA_FALLBACK_SKIP_PREFIXES: &[&str] = &["/api", "/artifacts", "/frames", "/_next"];

/// The request facts the fallback rule needs (parity:
/// `StaticSpaFallbackRequestLike`).
pub struct SpaFallbackRequest<'a> {
    pub method: &'a Method,
    pub path: &'a str,
    /// Missing header and empty header are the same input on both sides.
    pub accept: Option<&'a str>,
}

/// True when an unmatched path should answer `index.html` instead of a 404
/// (parity: `isStaticSpaFallbackRequest`).
pub fn is_static_spa_fallback_request(request: &SpaFallbackRequest<'_>) -> bool {
    if request.method != Method::GET && request.method != Method::HEAD {
        return false;
    }
    if SPA_FALLBACK_SKIP_PREFIXES
        .iter()
        .any(|prefix| matches_route_prefix(request.path, prefix))
    {
        return false;
    }
    match request.accept {
        None => true,
        Some(accept) => {
            accept.is_empty() || accept.contains("text/html") || accept.contains("*/*")
        }
    }
}

/// `path === prefix || path.startsWith(prefix + '/')` without allocating.
fn matches_route_prefix(path: &str, prefix: &str) -> bool {
    match path.strip_prefix(prefix) {
        Some(rest) => rest.is_empty() || rest.starts_with('/'),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decide(method: &str, path: &str, accept: Option<&str>) -> bool {
        let method = Method::from_bytes(method.as_bytes()).expect("method");
        is_static_spa_fallback_request(&SpaFallbackRequest {
            method: &method,
            path,
            accept,
        })
    }

    #[test]
    fn deep_links_with_html_accept_get_the_shell() {
        assert!(decide("GET", "/", Some("text/html")));
        assert!(decide("GET", "/projects/p1/files/a.png", Some("text/html,*/*")));
        assert!(decide("HEAD", "/settings", Some("text/html")));
    }

    #[test]
    fn missing_header_and_wildcard_accept_get_the_shell() {
        assert!(decide("GET", "/anything", None));
        assert!(decide("GET", "/anything", Some("")));
        assert!(decide("GET", "/anything", Some("*/*")));
    }

    #[test]
    fn non_get_methods_never_get_the_shell() {
        assert!(!decide("POST", "/", Some("text/html")));
        assert!(!decide("DELETE", "/x", Some("text/html")));
    }

    #[test]
    fn asset_and_api_prefixes_never_get_the_shell() {
        for path in [
            "/api",
            "/api/health",
            "/artifacts",
            "/artifacts/x.png",
            "/frames",
            "/frames/iphone-15-pro.html",
            "/_next",
            "/_next/static/chunks/main.js",
        ] {
            assert!(
                !decide("GET", path, Some("text/html")),
                "{path} must not fall back to the shell"
            );
        }
    }

    #[test]
    fn a_longer_sibling_of_a_skip_prefix_still_gets_the_shell() {
        // Parity: the TypeScript check is a segment match, not a bare
        // `startsWith`, so `/apiX` is an ordinary app path.
        assert!(decide("GET", "/apiX", Some("text/html")));
        assert!(decide("GET", "/artifacts-demo", Some("text/html")));
    }

    #[test]
    fn asset_like_accepts_get_a_404() {
        assert!(!decide("GET", "/x", Some("application/json")));
        assert!(!decide("GET", "/x", Some("image/png")));
    }
}
