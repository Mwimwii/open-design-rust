//! Integration tests for the diagnostics export surface.
//!
//! The bundle's internals (redaction, sources, zip writer, summaries) are pinned
//! by fixture tests inside `crates/od-daemon/src/diagnostics/`; these tests drive
//! the real HTTP routes with real files on disk, so the wiring — status codes,
//! headers, the entry roster inside the zip, and the renderer rendezvous — is
//! covered end to end.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use od_core::RuntimePaths;
use od_daemon::diagnostics::zip::read_zip_entries;
use od_daemon::{AppState, DaemonConfig, Store};
use serde_json::Value;
use tower::ServiceExt;

fn temp_paths(tag: &str) -> RuntimePaths {
    let dir = std::env::temp_dir().join(format!("od-diag-http-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    RuntimePaths::resolve(dir).expect("resolve data dir")
}

fn router_for(paths: RuntimePaths) -> axum::Router {
    let config = DaemonConfig {
        paths,
        bind_host: "127.0.0.1".to_string(),
        port: 0,
        api_token: None,
        api_auth_disabled: false,
        web_dist: None,
        desktop_auth_secret: None,
    };
    let store = Store::open(&config.paths.data_dir().join("app.sqlite")).expect("store");
    od_daemon::routes::build_router(AppState::new(config, store))
}

async fn call(
    router: axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut builder = Request::builder().method(method).uri(uri);
    let request = match body {
        Some(value) => {
            builder = builder.header("content-type", "application/json");
            builder.body(Body::from(value.to_string())).expect("request")
        }
        None => builder.body(Body::empty()).expect("request"),
    };
    let response = router.oneshot(request).await.expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, headers, bytes.to_vec())
}

fn json_entry(entries: &[(String, Vec<u8>)], name: &str) -> Value {
    let (_, bytes) = entries
        .iter()
        .find(|(entry, _)| entry == name)
        .unwrap_or_else(|| panic!("{name} missing from {:?}", names(entries)));
    serde_json::from_slice(bytes).expect("json entry")
}

fn names(entries: &[(String, Vec<u8>)]) -> Vec<&str> {
    entries.iter().map(|(name, _)| name.as_str()).collect()
}

fn read_entries(zip: &[u8]) -> Vec<(String, Vec<u8>)> {
    read_zip_entries(zip)
        .expect("read archive")
        .into_iter()
        .map(|entry| (entry.name, entry.data))
        .collect()
}

/// The data root, the run-event fixture and the export, all in one place.
fn fixture_paths(tag: &str) -> RuntimePaths {
    let paths = temp_paths(tag);
    let run_dir = paths.runs_dir().join("run-1");
    std::fs::create_dir_all(&run_dir).expect("runs dir");
    std::fs::write(
        run_dir.join("events.jsonl"),
        "{\"type\":\"text\",\"token\":\"secret-token\"}\n",
    )
    .expect("events");
    paths
}

#[tokio::test]
async fn diagnostics_export_returns_a_bundle_with_the_expected_roster() {
    let router = router_for(fixture_paths("shape"));
    let (status, headers, bytes) = call(router, "GET", "/api/diagnostics/export", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/zip");
    assert_eq!(headers["cache-control"], "no-store");
    let disposition = headers["content-disposition"]
        .to_str()
        .expect("content-disposition");
    assert!(disposition.starts_with("attachment; filename=\"open-design-diagnostics-"), "{disposition}");
    assert!(disposition.ends_with("Z.zip\""), "{disposition}");

    let entries = read_entries(&bytes);
    for expected in [
        "runs/run-1/events.jsonl",
        "summary/manifest.json",
        "summary/machine-info.json",
        "summary/automatic-log-upload.json",
        "summary/environment-evidence.json",
        "summary/chat-scroll-forensics.json",
        "summary/recent-api-failures.json",
        "summary/runtime-health.json",
    ] {
        assert!(names(&entries).contains(&expected), "{expected} missing from {:?}", names(&entries));
    }

    // A body was written for every listed source, never only in the manifest.
    let (_, log_bytes) = entries
        .iter()
        .find(|(name, _)| name == "runs/run-1/events.jsonl")
        .expect("run events entry");
    let log_text = String::from_utf8(log_bytes.clone()).expect("utf-8");
    assert!(log_text.contains("[REDACTED]"), "{log_text}");
    assert!(!log_text.contains("secret-token"), "{log_text}");

    let manifest = json_entry(&entries, "summary/manifest.json");
    assert_eq!(manifest["app"]["name"], "open-design");
    assert!(manifest["app"]["version"].is_string());
    assert_eq!(manifest["source"], "daemon-http");
    // Standalone launch: no sidecar log tree, and the manifest says so rather
    // than silently omitting daemon/web/desktop logs.
    assert!(
        manifest["warnings"]
            .as_array()
            .expect("warnings")
            .iter()
            .any(|warning| warning.as_str().unwrap_or_default().contains("standalone launch")),
        "{manifest}"
    );
    assert!(
        manifest["warnings"]
            .as_array()
            .expect("warnings")
            .iter()
            .any(|warning| warning.as_str().unwrap_or_default().contains("conversation content")),
        "{manifest}"
    );
    // Every collected source is listed with its byte count. The agent-CLI log
    // sweep reads the real home directory, so the count is not fixed; the
    // run-event fixture must always be in there.
    let files = manifest["files"].as_array().expect("files");
    assert!(
        files.iter().any(|file| file["name"] == "runs/run-1/events.jsonl"),
        "{manifest}"
    );
    for file in files {
        assert!(file["name"].is_string(), "{file}");
        assert!(file["absolutePath"].is_string(), "{file}");
        assert!(file["bytes"].is_u64(), "{file}");
    }
    assert!(manifest.get("namespace").is_none());

    let machine = json_entry(&entries, "summary/machine-info.json");
    assert_eq!(machine["hostname"], "[REDACTED]");
    assert!(machine["platform"].is_string());
    assert!(machine["nodeVersion"].is_string());

    let health = json_entry(&entries, "summary/runtime-health.json");
    assert_eq!(health["daemon"]["reachable"], true);
    assert_eq!(health["coverage"]["runEventsPresent"], true);
}

#[tokio::test]
async fn a_chat_scroll_capture_round_trips_into_the_bundle() {
    let router = router_for(temp_paths("chat-scroll"));
    let (status, _, body) = call(
        router.clone(),
        "POST",
        "/api/diagnostics/chat-scroll-forensics",
        Some(serde_json::json!({ "chatLogHtml": "<div>stuck</div>", "scrollTop": 120 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ack: Value = serde_json::from_slice(&body).expect("ack json");
    assert_eq!(ack["ok"], true);
    assert!(ack["receivedAtIso"].is_string());

    let (status, _, bytes) = call(router, "GET", "/api/diagnostics/export", None).await;
    assert_eq!(status, StatusCode::OK);
    let entries = read_entries(&bytes);
    let summary = json_entry(&entries, "summary/chat-scroll-forensics.json");
    assert_eq!(summary["captured"], true);
    assert_eq!(summary["evidence"]["chatLogHtml"], "<div>stuck</div>");
    assert!(summary["receivedAtIso"].is_string());
}

#[tokio::test]
async fn a_bundle_without_a_capture_states_the_absence_in_words() {
    let router = router_for(temp_paths("no-chat-scroll"));
    let (status, _, bytes) = call(router, "GET", "/api/diagnostics/export", None).await;
    assert_eq!(status, StatusCode::OK);
    let entries = read_entries(&bytes);
    let summary = json_entry(&entries, "summary/chat-scroll-forensics.json");
    assert_eq!(summary["captured"], false);
    assert_eq!(summary["evidence"], Value::Null);
    assert!(
        summary["note"]
            .as_str()
            .unwrap_or_default()
            .contains("No chat-scroll capture"),
        "{summary}"
    );
}

#[tokio::test]
async fn the_chat_scroll_route_rejects_a_non_object_body() {
    let router = router_for(temp_paths("chat-scroll-bad"));
    let (status, _, body) = call(
        router,
        "POST",
        "/api/diagnostics/chat-scroll-forensics",
        Some(serde_json::json!([1, 2, 3])),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let parsed: Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(parsed["error"], "INVALID_CHAT_SCROLL_FORENSICS");
}

#[tokio::test]
async fn failed_api_responses_reach_both_failure_summaries() {
    let router = router_for(temp_paths("api-failures"));
    let (status, _, _) = call(router.clone(), "GET", "/api/definitely-not-a-route", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _, bytes) = call(router, "GET", "/api/diagnostics/export", None).await;
    assert_eq!(status, StatusCode::OK);
    let entries = read_entries(&bytes);

    let recent = json_entry(&entries, "summary/recent-api-failures.json");
    assert_eq!(recent["retainedLimit"], 100);
    let failures = recent["failures"].as_array().expect("failures");
    assert_eq!(failures.len(), 1, "{recent}");
    assert_eq!(failures[0]["path"], "/api/:unmatched");
    assert_eq!(failures[0]["method"], "GET");
    assert_eq!(failures[0]["status"], 404);
    assert_eq!(failures[0]["retryable"], false);
    // Privacy: no query string, body or resource identifier survives.
    assert!(failures[0].get("query").is_none());

    let evidence = json_entry(&entries, "summary/environment-evidence.json");
    assert_eq!(evidence["schemaVersion"], 1);
    assert_eq!(evidence["coverage"]["persistence"], "memory-only");
    let groups = evidence["failures"].as_array().expect("groups");
    assert_eq!(groups.len(), 1, "{evidence}");
    assert_eq!(groups[0]["operation"], "/api/:unmatched");
    assert_eq!(groups[0]["code"], "http");
    assert_eq!(groups[0]["status"], 404);
    assert_eq!(groups[0]["count"], 1);
    assert_eq!(groups[0]["source"], "local-api");
}
