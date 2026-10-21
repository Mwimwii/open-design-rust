//! Guarded route registration (parity:
//! `apps/daemon/src/route-registration-guard.ts`).
//!
//! Two extracted, security-sensitive routes must be registered exactly once:
//! a second registration silently shadows the first in Express, and in axum it
//! aborts router construction with an opaque "overlapping method route" panic.
//! This module keeps the TS key shape (and its exact error message) so a route
//! module can state the invariant at its registration site.
//!
//! Status in the Rust port: neither guarded route is ported yet
//! (`POST /api/projects/:id/export/pdf` and
//! `POST /api/projects/:id/media/generate`), so this guard is dormant. When
//! either lands it must register through [`RouteRegistrationGuard::register`];
//! the tests below pin the behavior it will enforce.
//!
//! The inventory mirrors `getRouteRegistrationInventory`: every registration
//! seen, in order, with the HTTP method upper-cased.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteRegistration {
    pub method: String,
    pub path: String,
}

const GUARDED_ROUTE_KEYS: [&str; 2] = [
    "POST /api/projects/:id/export/pdf",
    "POST /api/projects/:id/media/generate",
];

/// Express uses `:id`; axum uses `{id}`. Normalize to the TS spelling so the
/// guarded key and its error message are identical on both sides.
fn normalize_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut rest = path;
    while let Some(start) = rest.find('{') {
        let (before, after_start) = rest.split_at(start);
        out.push_str(before);
        let Some(end) = after_start.find('}') else {
            out.push_str(after_start);
            return out;
        };
        let (segment, after) = after_start.split_at(end + 1);
        let name = &segment[1..segment.len() - 1];
        if name.is_empty() || name == "*rest" || name.starts_with('*') {
            out.push_str(segment);
        } else {
            out.push(':');
            out.push_str(name);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// The guarded key for a `(method, path)` pair, or `None` when the route is not
/// guarded. Mirrors `guardedRouteKey`.
pub fn guarded_route_key(method: &str, path: &str) -> Option<String> {
    let key = format!("{} {}", method.to_uppercase(), normalize_path(path));
    GUARDED_ROUTE_KEYS.contains(&key.as_str()).then_some(key)
}

/// Registration-time guard: records every route and rejects a second
/// registration of a guarded one with the TS message.
#[derive(Debug, Default)]
pub struct RouteRegistrationGuard {
    seen: std::collections::HashSet<String>,
    inventory: Vec<RouteRegistration>,
}

impl RouteRegistrationGuard {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, method: &str, path: &str) -> Result<(), String> {
        self.inventory.push(RouteRegistration {
            method: method.to_uppercase(),
            path: normalize_path(path),
        });
        let Some(key) = guarded_route_key(method, path) else {
            return Ok(());
        };
        if !self.seen.insert(key.clone()) {
            return Err(format!("duplicate guarded route registration: {key}"));
        }
        Ok(())
    }

    pub fn inventory(&self) -> Vec<RouteRegistration> {
        self.inventory.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_the_sensitive_extracted_routes() {
        assert_eq!(
            guarded_route_key("post", "/api/projects/:id/export/pdf").as_deref(),
            Some("POST /api/projects/:id/export/pdf")
        );
        assert_eq!(
            guarded_route_key("post", "/api/projects/{id}/media/generate").as_deref(),
            Some("POST /api/projects/:id/media/generate")
        );
        assert_eq!(guarded_route_key("get", "/api/projects/:id/export/pdf"), None);
        assert_eq!(guarded_route_key("post", "/api/runs"), None);
    }

    #[test]
    fn throws_when_a_guarded_route_is_registered_twice() {
        let mut guard = RouteRegistrationGuard::new();
        guard
            .register("post", "/api/projects/{id}/media/generate")
            .expect("first registration");
        let error = guard
            .register("POST", "/api/projects/:id/media/generate")
            .expect_err("duplicate must fail");
        assert_eq!(
            error,
            "duplicate guarded route registration: POST /api/projects/:id/media/generate"
        );
    }

    #[test]
    fn does_not_blanket_reject_non_sensitive_duplicates() {
        let mut guard = RouteRegistrationGuard::new();
        guard.register("get", "/api/runs").expect("first");
        guard.register("get", "/api/runs").expect("second");
        assert_eq!(guard.inventory().len(), 2);
    }

    #[test]
    fn inventory_keeps_registration_order_and_upper_case_methods() {
        let mut guard = RouteRegistrationGuard::new();
        guard.register("get", "/api/health").expect("health");
        guard.register("post", "/api/projects/{id}/export/pdf").expect("pdf");
        assert_eq!(
            guard.inventory(),
            vec![
                RouteRegistration {
                    method: "GET".to_string(),
                    path: "/api/health".to_string(),
                },
                RouteRegistration {
                    method: "POST".to_string(),
                    path: "/api/projects/:id/export/pdf".to_string(),
                },
            ]
        );
    }

    #[test]
    fn wildcard_segments_are_not_renamed() {
        assert_eq!(normalize_path("/api/{*rest}"), "/api/{*rest}");
        assert_eq!(normalize_path("/api/projects/{id}/files/{*path}"), "/api/projects/:id/files/{*path}");
    }
}
