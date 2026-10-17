//! The `window.__od__` host bridge: the injected script *and* the IPC commands
//! behind it.
//!
//! The window's page is the `apps/web` static export served by the embedded
//! daemon, so it is a remote origin with no bundler to import a bridge module
//! from. Tauri injects [`bridge::script`] before the page's own scripts run,
//! and the app finds the bridge on its first render
//! (`isOpenDesignHostAvailable()` in `packages/host`).
//!
//! The split of responsibility is deliberate:
//!
//! * `bridge.rs` is a *pipe*. It marshals each method to one command and
//!   normalizes the answer into the wire shape `packages/host/src/protocol.ts`
//!   declares. It holds no policy — no path validation, no daemon URLs, no OS
//!   interaction.
//! * The modules beside it are the host. Each command answers the bridge
//!   envelope for its method, so the shape of the contract is reviewable in
//!   Rust, where the types, the daemon client and the OS calls live.
//!
//! Every command is declared twice, on purpose: `build.rs` lists it for
//! `tauri_build::AppManifest` (which generates the `allow-<command>`
//! permissions) and `capabilities/default.json` grants those permissions. A
//! remote origin gets no command that is absent from either list.

pub mod bridge;
pub mod desktop_auth;
pub mod project;
pub mod shell;

use serde_json::{json, Value};

use desktop_auth::DESKTOP_IMPORT_TOKEN_HEADER;

/// The wire failure envelope (`OpenDesignHostFailure` in `packages/host`).
pub fn failure(reason: impl Into<String>) -> Value {
    json!({ "ok": false, "reason": reason.into() })
}

/// The bare success envelope (`OpenDesignHostActionResult`).
pub fn success() -> Value {
    json!({ "ok": true })
}

/// Parity: `isSafeId` / the `[A-Za-z0-9._-]{1,128}` floor the Electron host
/// applies before it builds a daemon URL from a renderer-supplied project id.
pub fn is_safe_project_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// `GET /api/projects/:id` on the embedded daemon.
///
/// The shell and the daemon share a loopback origin, and loopback peers are
/// unauthenticated by design (`od_daemon::auth`: the desktop UI never carries
/// credentials), so this needs no token.
///
/// The `Err` strings are the Electron host's, because they reach the renderer
/// through the same bridge contract.
pub async fn daemon_get_project(daemon_url: &str, project_id: &str) -> Result<Value, String> {
    let url = format!(
        "{}/api/projects/{}",
        daemon_url.trim_end_matches('/'),
        project_id
    );
    let response = reqwest::get(&url)
        .await
        .map_err(|err| format!("daemon fetch failed: {err}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("daemon returned HTTP {status}"));
    }
    response
        .json::<Value>()
        .await
        .map_err(|_| "daemon response was not JSON".to_string())
}

/// `POST <path>` on the embedded daemon with a desktop import token.
///
/// Answers `(status, body)` for every HTTP response, including error ones,
/// because the bridge contract carries the daemon's status and body into the
/// renderer's failure envelope. `Err` is reserved for "the daemon could not be
/// reached at all" — the same split the Electron host uses.
pub async fn daemon_post_json(
    daemon_url: &str,
    path: &str,
    import_token: &str,
    body: Value,
    headers: Vec<(String, String)>,
) -> Result<(u16, Value), String> {
    let url = format!("{}{}", daemon_url.trim_end_matches('/'), path);
    let mut request = reqwest::Client::new()
        .post(&url)
        .header("Content-Type", "application/json")
        .header(DESKTOP_IMPORT_TOKEN_HEADER, import_token)
        .json(&body);
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let response = request
        .send()
        .await
        .map_err(|err| format!("daemon fetch failed: {err}"))?;
    let status = response.status().as_u16();
    let text = response
        .text()
        .await
        .map_err(|err| format!("daemon fetch failed: {err}"))?;
    // A non-JSON body (an HTML error page, say) is `null`, which the caller
    // reports without `details` — parity with the Electron host.
    Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
}

/// The daemon's project metadata, whichever shape the daemon serializes.
///
/// The TypeScript daemon returns `project.metadata` as an object (see
/// `normalizeProject`), and every reader in `apps/web` reads it that way. The
/// Rust daemon currently returns the raw `metadata_json` string column
/// instead, so both are read here: the trust checks below must not become
/// skippable because the daemon's shape drifted. Fixing the daemon's
/// serialization is tracked as its own bead.
pub fn project_metadata(project: &Value) -> Option<Value> {
    match project.get("metadata") {
        Some(Value::Object(_)) => project.get("metadata").cloned(),
        _ => project
            .get("metadata_json")
            .and_then(Value::as_str)
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn safe_project_id_matches_the_electron_floor() {
        assert!(is_safe_project_id("project-1"));
        assert!(is_safe_project_id("my.project.v2"));
        assert!(is_safe_project_id("a_b-c.1"));
        let long = "a".repeat(128);
        assert!(is_safe_project_id(&long));
    }

    #[test]
    fn unsafe_project_ids_are_rejected() {
        assert!(!is_safe_project_id(""));
        assert!(!is_safe_project_id("a/b"));
        assert!(!is_safe_project_id("a b"));
        assert!(!is_safe_project_id("../etc"));
        assert!(!is_safe_project_id("a?b"));
        assert!(!is_safe_project_id(&"a".repeat(129)));
    }

    #[test]
    fn metadata_reads_an_object_or_a_json_string() {
        let object = json!({ "project": { "metadata": { "baseDir": "/tmp/x" } } });
        assert_eq!(
            project_metadata(&object["project"]).and_then(|m| m.get("baseDir").cloned()),
            Some(json!("/tmp/x"))
        );

        let string_column = json!({
            "project": { "metadata_json": "{\"baseDir\":\"/tmp/x\",\"fromTrustedPicker\":true}" }
        });
        let metadata = project_metadata(&string_column["project"]).expect("parsed");
        assert_eq!(metadata.get("baseDir"), Some(&json!("/tmp/x")));
        assert_eq!(metadata.get("fromTrustedPicker"), Some(&json!(true)));
    }

    #[test]
    fn metadata_is_none_when_absent_or_unparsable() {
        assert!(project_metadata(&json!({})).is_none());
        assert!(project_metadata(&json!({ "metadata_json": "not json" })).is_none());
        assert!(project_metadata(&json!({ "metadata": null })).is_none());
    }
}
