//! Integration tests for the static skill-resource routes (parity with
//! `apps/daemon/src/routes/static-resource.ts`'s `/example` and `/assets`
//! routes) and the chat artifact read routes (parity with
//! `apps/daemon/src/routes/project/chat-artifacts.ts`).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use axum::body::{to_bytes, Body};
use axum::http::{header, HeaderMap, HeaderName, Request, StatusCode};
use axum::Router;
use od_core::RuntimePaths;
use od_daemon::routes::build_router;
use od_daemon::{AppState, DaemonConfig, Store};
use serde_json::Value;
use tower::ServiceExt;

const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\nod-png-fixture";
const THUMB_BYTES: &[u8] = b"\x89PNG\r\n\x1a\nod-thumb-fixture";
const SVG_BYTES: &[u8] = b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>";

struct Fixture {
    router: Router,
    store: Store,
    data_root: PathBuf,
}

fn setup(tag: &str) -> Fixture {
    setup_with_token(tag, None)
}

fn setup_with_token(tag: &str, api_token: Option<&str>) -> Fixture {
    let data_root =
        std::env::temp_dir().join(format!("od-daemon-artifacts-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_root);
    let paths = RuntimePaths::resolve(&data_root).expect("data dir");
    let config = DaemonConfig {
        paths,
        bind_host: "127.0.0.1".to_string(),
        port: 0,
        api_token: api_token.map(str::to_string),
        api_auth_disabled: false,
        web_dist: None,
    };
    let store = Store::open(&config.paths.db_file()).expect("store");
    let router = build_router(AppState::new(config, store.clone()));
    Fixture {
        router,
        store,
        data_root,
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.data_root);
    }
}

fn loopback() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

async fn get_with(
    router: &Router,
    uri: &str,
    headers: &[(HeaderName, &str)],
    peer: Option<SocketAddr>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(peer) = peer {
        builder = builder.extension(axum::extract::ConnectInfo(peer));
    }
    for (name, value) in headers {
        builder = builder.header(name.clone(), *value);
    }
    let request = builder.body(Body::empty()).expect("request");
    let response = router.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let response_headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, response_headers, body.to_vec())
}

async fn get(router: &Router, uri: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
    get_with(router, uri, &[], Some(loopback())).await
}

fn json(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap_or_else(|err| panic!("json body {err}: {body:?}"))
}

fn content_type(headers: &HeaderMap) -> &str {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
}

fn header_value(headers: &HeaderMap, name: HeaderName) -> Option<&str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn no_store(headers: &HeaderMap) -> bool {
    header_value(headers, header::CACHE_CONTROL) == Some("no-store")
}

// ---- static skill resources ------------------------------------------------

/// Seed `<data>/skills/<folder>` with a `SKILL.md` (frontmatter `name` when
/// given) plus the listed files, paths relative to the skill folder.
fn seed_skill(data_root: &Path, folder: &str, skill_id: Option<&str>, files: &[(&str, &str)]) {
    let dir = data_root.join("skills").join(folder);
    std::fs::create_dir_all(&dir).expect("skill dir");
    let skill_md = match skill_id {
        Some(id) => format!("---\nname: {id}\n---\n# {folder}\n"),
        None => format!("# {folder}\n"),
    };
    std::fs::write(dir.join("SKILL.md"), skill_md).expect("SKILL.md");
    for (rel, contents) in files {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("file dir");
        }
        std::fs::write(path, contents).expect("file");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skill_example_serves_baked_html_and_rewrites_asset_urls() {
    let fixture = setup("example-baked");
    seed_skill(
        &fixture.data_root,
        "demo",
        Some("demo-skill"),
        &[(
            "example.html",
            "<html><head><title>Demo</title></head><body>\
             <img src=\"./assets/logo.png\">\
             <a href=\"../other/assets/a.svg\">o</a>\
             </body></html>",
        )],
    );

    let (status, headers, body) = get(&fixture.router, "/api/skills/demo-skill/example").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), "text/html; charset=utf-8");
    let html = String::from_utf8_lossy(&body);
    assert!(
        html.contains(r#"src="/api/skills/demo-skill/assets/logo.png""#),
        "{html}"
    );
    assert!(
        html.contains(r#"href="/api/skills/other/assets/a.svg""#),
        "{html}"
    );
    assert!(html.contains("<title>Demo</title>"), "{html}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skill_example_resolves_alias_ids() {
    let fixture = setup("example-alias");
    seed_skill(
        &fixture.data_root,
        "taste",
        Some("design-taste-frontend"),
        &[("example.html", "ALIAS-EXAMPLE")],
    );

    let (status, _, body) = get(&fixture.router, "/api/skills/taste-skill/example").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(String::from_utf8_lossy(&body), "ALIAS-EXAMPLE");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skill_example_derived_id_serves_the_named_example() {
    let fixture = setup("example-derived");
    seed_skill(
        &fixture.data_root,
        "parent",
        Some("parent-skill"),
        &[(
            "examples/child.html",
            r#"<img src="./assets/x.png">CHILD-EXAMPLE"#,
        )],
    );

    let (status, _, body) = get(&fixture.router, "/api/skills/parent-skill:child/example").await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("CHILD-EXAMPLE"), "{html}");
    assert!(
        html.contains(r#"src="/api/skills/parent-skill/assets/x.png""#),
        "{html}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skill_example_assembles_template_and_slides() {
    let fixture = setup("example-assemble");
    seed_skill(
        &fixture.data_root,
        "asm",
        Some("asm-skill"),
        &[
            (
                "assets/template.html",
                "<html><head><title>Seed</title></head><body>\
                 <script>var s = '<title>Fake</title>';</script>\
                 <!-- SLIDES_HERE --></body></html>",
            ),
            ("assets/example-slides.html", "<section>ONE</section>"),
        ],
    );

    let (status, _, body) = get(&fixture.router, "/api/skills/asm-skill/example").await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8_lossy(&body);
    assert!(
        html.contains("<title>asm-skill | OpenDesign Example</title>"),
        "{html}"
    );
    assert!(html.contains("<section>ONE</section>"), "{html}");
    assert!(html.contains("<title>Fake</title>"), "{html}");
    assert!(!html.contains("SLIDES_HERE"), "{html}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skill_example_falls_back_to_the_first_examples_html() {
    let fixture = setup("example-fallback");
    seed_skill(
        &fixture.data_root,
        "fb",
        Some("fb-skill"),
        &[
            ("examples/beta.html", "BETA"),
            ("examples/alpha.html", "ALPHA"),
        ],
    );

    let (status, _, body) = get(&fixture.router, "/api/skills/fb-skill/example").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(String::from_utf8_lossy(&body), "ALPHA");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skill_example_missing_cases_404() {
    let fixture = setup("example-404");
    seed_skill(&fixture.data_root, "bare", Some("bare-skill"), &[]);

    let (status, _, body) = get(&fixture.router, "/api/skills/ghost/example").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json(&body)["error"]["code"], "NOT_FOUND");
    assert_eq!(json(&body)["error"]["message"], "skill not found");

    let (status, _, body) = get(&fixture.router, "/api/skills/bare-skill/example").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        json(&body)["error"]["message"],
        "no example.html, assets/template.html, assets/index.html, or examples/*.html for this skill"
    );

    let (status, _, body) = get(&fixture.router, "/api/skills/ghost:child/example").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json(&body)["error"]["message"], "skill not found");

    let (status, _, body) = get(&fixture.router, "/api/skills/bare-skill:child/example").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json(&body)["error"]["message"], "derived example not found");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skill_assets_serve_bytes_mime_and_origin_null_cors() {
    let fixture = setup("assets-happy");
    seed_skill(
        &fixture.data_root,
        "demo",
        Some("demo-skill"),
        &[("assets/logo.png", "PNG")],
    );

    let (status, headers, body) = get(&fixture.router, "/api/skills/demo-skill/assets/logo.png").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), "image/png");
    assert_eq!(body, b"PNG");
    assert!(header_value(&headers, header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());

    let (status, headers, body) = get_with(
        &fixture.router,
        "/api/skills/demo-skill/assets/logo.png",
        &[(header::ORIGIN, "null")],
        Some(loopback()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"PNG");
    assert_eq!(
        header_value(&headers, header::ACCESS_CONTROL_ALLOW_ORIGIN),
        Some("*")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skill_assets_refuse_traversal_symlinks_and_missing_files() {
    let fixture = setup("assets-refuse");
    seed_skill(
        &fixture.data_root,
        "demo",
        Some("demo-skill"),
        &[("assets/logo.png", "PNG")],
    );
    std::fs::write(fixture.data_root.join("secret.txt"), "top secret").unwrap();

    // Lexical traversal is refused before any existence check.
    for uri in [
        "/api/skills/demo-skill/assets/..%2F..%2Fsecret.txt",
        "/api/skills/demo-skill/assets/%2Fetc%2Fpasswd",
    ] {
        let (status, _, body) = get(&fixture.router, uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body:?}");
        assert_eq!(json(&body)["error"]["message"], "invalid asset path", "{uri}");
    }

    // A symlink out of the skill folder is refused rather than followed.
    #[cfg(unix)]
    {
        let escape = fixture
            .data_root
            .join("skills/demo/assets/escape.png");
        std::os::unix::fs::symlink(fixture.data_root.join("secret.txt"), &escape).unwrap();
        let (status, _, body) = get(&fixture.router, "/api/skills/demo-skill/assets/escape.png").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
        assert_eq!(json(&body)["error"]["message"], "invalid asset path");
    }

    let (status, _, body) = get(&fixture.router, "/api/skills/demo-skill/assets/missing.png").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json(&body)["error"]["message"], "asset not found");

    let (status, _, body) = get(&fixture.router, "/api/skills/ghost/assets/logo.png").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json(&body)["error"]["message"], "skill not found");

    // The secret itself stays unreachable.
    let (status, _, _) = get(&fixture.router, "/api/skills/demo-skill/assets/secret.txt").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---- chat artifacts --------------------------------------------------------

fn content_hex() -> String {
    "0f".repeat(32)
}

fn content_digest() -> String {
    format!("sha256:{}", content_hex())
}

fn content_key() -> String {
    format!("objects/0f/0f/{}", content_hex())
}

fn thumb_hex() -> String {
    "1e".repeat(32)
}

fn thumb_digest() -> String {
    format!("sha256:{}", thumb_hex())
}

fn thumb_key() -> String {
    format!("objects/1e/1e/{}", thumb_hex())
}

fn other_digest(prefix: &str) -> String {
    format!("sha256:{}", prefix.repeat(32))
}

fn other_key(prefix: &str) -> String {
    format!("objects/{prefix}/{prefix}/{}", prefix.repeat(32))
}

fn write_blob(data_root: &Path, key: &str, bytes: &[u8]) {
    let path = data_root.join("chat-artifact-blobs").join(key);
    std::fs::create_dir_all(path.parent().expect("blob parent")).expect("blob dir");
    std::fs::write(path, bytes).expect("blob file");
}

#[allow(clippy::too_many_arguments)]
fn insert_snapshot(
    store: &Store,
    id: &str,
    project_id: &str,
    workspace_artifact_id: Option<&str>,
    source: &str,
    kind: &str,
    mime: Option<&str>,
    content_digest: Option<&str>,
    thumbnail_digest: Option<&str>,
    state: &str,
    failure_code: Option<&str>,
    run_id: Option<&str>,
    media_task_id: Option<&str>,
    ready_at: Option<i64>,
) {
    store
        .execute(
            "INSERT INTO chat_artifact_snapshots
               (id, project_id, workspace_artifact_id, source_path_at_capture, kind, mime,
                content_digest, thumbnail_digest, run_id, media_task_id,
                capture_state, failure_code, created_at, ready_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 1700000000000, ?13)",
            rusqlite::params![
                id,
                project_id,
                workspace_artifact_id,
                source,
                kind,
                mime,
                content_digest,
                thumbnail_digest,
                run_id,
                media_task_id,
                state,
                failure_code,
                ready_at
            ],
        )
        .expect("insert snapshot");
}

fn seed_chat_artifacts(fixture: &Fixture) {
    let store = &fixture.store;

    for (id, name, metadata) in [
        ("p1", "Demo", None),
        ("p2", "Other", None),
        (
            "p-revoked",
            "Revoked",
            Some(r#"{"teamMirrorRevokedAt":1700000000000}"#),
        ),
    ] {
        store
            .execute(
                "INSERT INTO projects (id, name, metadata_json, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 1, 2)",
                rusqlite::params![id, name, metadata],
            )
            .expect("insert project");
    }

    for id in ["c1", "c2"] {
        store
            .execute(
                "INSERT INTO conversations (id, project_id, created_at, updated_at)
                 VALUES (?1, 'p1', 1, 2)",
                [id],
            )
            .expect("insert conversation");
    }
    store
        .execute(
            "INSERT INTO messages (id, conversation_id, role, content, position, created_at)
             VALUES ('m1', 'c1', 'user', 'hi', 0, 1)",
            (),
        )
        .expect("insert m1");
    store
        .execute(
            "INSERT INTO messages (id, conversation_id, role, content, position, created_at)
             VALUES ('m2', 'c2', 'user', 'hi', 0, 1)",
            (),
        )
        .expect("insert m2");

    store
        .execute(
            "INSERT INTO workspace_artifacts
               (id, project_id, current_path, kind, mime, current_digest, current_size,
                current_mtime, created_at, updated_at, deleted_at)
             VALUES ('wa1', 'p1', 'Design Files/hero.png', 'image', 'image/png', ?1, ?2,
                     ?3, 1, 2, NULL)",
            rusqlite::params![content_digest(), PNG_BYTES.len() as i64, 1_700_000_100_000_i64],
        )
        .expect("insert wa1");
    store
        .execute(
            "INSERT INTO workspace_artifacts
               (id, project_id, current_path, kind, mime, current_digest, current_size,
                current_mtime, created_at, updated_at, deleted_at)
             VALUES ('wt1', 'p1', NULL, 'image', NULL, NULL, NULL, NULL, 1, 2, 1700000000000)",
            (),
        )
        .expect("insert wt1");
    store
        .execute(
            "INSERT INTO workspace_artifacts
               (id, project_id, current_path, kind, mime, current_digest, current_size,
                current_mtime, created_at, updated_at, deleted_at)
             VALUES ('wa-other', 'p2', 'Design Files/other.png', 'image', NULL, NULL, NULL,
                     NULL, 1, 2, NULL)",
            (),
        )
        .expect("insert wa-other");

    let blob_sql = "INSERT INTO chat_artifact_blobs
         (digest, storage_key, byte_size, mime, created_at, last_verified_at)
       VALUES (?1, ?2, ?3, ?4, 1, 2)";
    store
        .execute(
            blob_sql,
            rusqlite::params![
                content_digest(),
                content_key(),
                PNG_BYTES.len() as i64,
                "image/png"
            ],
        )
        .expect("insert content blob");
    store
        .execute(
            blob_sql,
            rusqlite::params![
                thumb_digest(),
                thumb_key(),
                THUMB_BYTES.len() as i64,
                "image/png"
            ],
        )
        .expect("insert thumb blob");
    store
        .execute(
            blob_sql,
            rusqlite::params![
                other_digest("5e"),
                other_key("5e"),
                SVG_BYTES.len() as i64,
                rusqlite::types::Null
            ],
        )
        .expect("insert svg blob");
    // Row claims 999 bytes; the file on disk holds 4.
    store
        .execute(
            blob_sql,
            rusqlite::params![other_digest("3c"), other_key("3c"), 999_i64, "image/png"],
        )
        .expect("insert mismatched blob");
    // Row points at a key that cannot resolve under the blob root.
    store
        .execute(
            blob_sql,
            rusqlite::params![
                other_digest("4d"),
                "../../etc/passwd",
                4_i64,
                "text/plain"
            ],
        )
        .expect("insert evil blob");

    write_blob(&fixture.data_root, &content_key(), PNG_BYTES);
    write_blob(&fixture.data_root, &thumb_key(), THUMB_BYTES);
    write_blob(&fixture.data_root, &other_key("5e"), SVG_BYTES);
    write_blob(&fixture.data_root, &other_key("3c"), b"abcd");

    insert_snapshot(
        store,
        "s1",
        "p1",
        Some("wa1"),
        "Design Files/hero image.png",
        "image",
        Some("image/png"),
        Some(&content_digest()),
        Some(&thumb_digest()),
        "ready",
        None,
        Some("run-1"),
        Some("task-1"),
        Some(1_700_000_000_500),
    );
    insert_snapshot(
        store,
        "s2",
        "p1",
        None,
        "Design Files/pending.png",
        "image",
        Some("image/png"),
        None,
        None,
        "pending",
        Some("source_missing"),
        None,
        None,
        None,
    );
    // Ready, but no blob row was ever written for its digest.
    insert_snapshot(
        store,
        "s3",
        "p1",
        None,
        "Design Files/gone.png",
        "image",
        Some("image/png"),
        Some(&other_digest("2b")),
        None,
        "ready",
        None,
        None,
        None,
        None,
    );
    insert_snapshot(
        store,
        "s4",
        "p1",
        None,
        "Design Files/mismatch.png",
        "image",
        Some("image/png"),
        Some(&other_digest("3c")),
        None,
        "ready",
        None,
        None,
        None,
        None,
    );
    insert_snapshot(
        store,
        "s5",
        "p1",
        None,
        "Design Files/evil.png",
        "image",
        Some("image/png"),
        Some(&other_digest("4d")),
        None,
        "ready",
        None,
        None,
        None,
        None,
    );
    insert_snapshot(
        store,
        "s6",
        "p1",
        None,
        "Design Files/pic.svg",
        "image",
        Some("image/svg+xml"),
        Some(&other_digest("5e")),
        None,
        "ready",
        None,
        None,
        None,
        None,
    );
    insert_snapshot(
        store,
        "s-other",
        "p2",
        None,
        "Design Files/other.png",
        "image",
        Some("image/png"),
        Some(&content_digest()),
        None,
        "ready",
        None,
        None,
        None,
        None,
    );

    store
        .execute(
            "INSERT INTO message_artifacts
               (message_id, ordinal, id, snapshot_id, workspace_artifact_id, display_policy,
                label_at_capture, kind, html_version_id, created_at)
             VALUES ('m1', 0, 'ma1', 's1', 'wa1', 'immutable_snapshot', 'hero.png', 'image',
                     NULL, 1)",
            (),
        )
        .expect("insert ma1");
    store
        .execute(
            "INSERT INTO message_artifacts
               (message_id, ordinal, id, snapshot_id, workspace_artifact_id, display_policy,
                label_at_capture, kind, html_version_id, created_at)
             VALUES ('m1', 1, 'ma2', 's2', NULL, 'latest_with_static_preview', 'pending.png',
                     'image', NULL, 1)",
            (),
        )
        .expect("insert ma2");
    store
        .execute(
            "INSERT INTO message_artifacts
               (message_id, ordinal, id, snapshot_id, workspace_artifact_id, display_policy,
                label_at_capture, kind, html_version_id, created_at)
             VALUES ('m1', 2, 'ma3', NULL, NULL, 'immutable_snapshot', 'legacy.png', 'binary',
                     NULL, 1)",
            (),
        )
        .expect("insert ma3");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn message_refs_project_the_artifact_rows() {
    let fixture = setup("refs-happy");
    seed_chat_artifacts(&fixture);

    let (status, headers, body) = get(
        &fixture.router,
        "/api/projects/p1/conversations/c1/messages/m1/artifacts",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), "application/json");
    assert!(no_store(&headers));

    let artifacts = json(&body)["artifacts"]
        .as_array()
        .expect("artifacts array")
        .clone();
    assert_eq!(artifacts.len(), 3);

    let ready = &artifacts[0];
    assert_eq!(ready["id"], "ma1");
    assert_eq!(ready["label"], "hero.png");
    assert_eq!(ready["kind"], "image");
    assert_eq!(ready["displayPolicy"], "immutable_snapshot");
    assert_eq!(ready["snapshotState"], "ready");
    assert_eq!(ready["workspaceArtifactId"], "wa1");
    assert_eq!(ready["snapshotId"], "s1");
    assert_eq!(
        ready["snapshotUrl"],
        "/api/projects/p1/chat-artifact-snapshots/s1/content"
    );
    assert_eq!(
        ready["thumbnailUrl"],
        "/api/projects/p1/chat-artifact-snapshots/s1/thumbnail"
    );

    let pending = &artifacts[1];
    assert_eq!(pending["id"], "ma2");
    assert_eq!(pending["snapshotState"], "pending");
    assert!(pending.get("snapshotId").is_none(), "{pending}");
    assert!(pending.get("snapshotUrl").is_none(), "{pending}");

    let legacy = &artifacts[2];
    assert_eq!(legacy["id"], "ma3");
    assert_eq!(legacy["snapshotState"], "legacy_unavailable");
    assert_eq!(legacy["kind"], "binary");
    assert!(legacy.get("snapshotId").is_none(), "{legacy}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn message_refs_404_for_unknown_or_mismatched_ids() {
    let fixture = setup("refs-404");
    seed_chat_artifacts(&fixture);

    for uri in [
        // Unknown message.
        "/api/projects/p1/conversations/c1/messages/ghost/artifacts",
        // Real message, wrong conversation.
        "/api/projects/p1/conversations/c1/messages/m2/artifacts",
        // Real message, wrong project.
        "/api/projects/p2/conversations/c1/messages/m1/artifacts",
        // Unknown conversation.
        "/api/projects/p1/conversations/ghost/messages/m1/artifacts",
    ] {
        let (status, headers, body) = get(&fixture.router, uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}: {body:?}");
        assert_eq!(json(&body)["error"]["code"], "NOT_FOUND", "{uri}");
        assert_eq!(json(&body)["error"]["message"], "message not found", "{uri}");
        assert!(!no_store(&headers), "{uri}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_metadata_projects_ready_fields() {
    let fixture = setup("snapshot-meta");
    seed_chat_artifacts(&fixture);

    let (status, headers, body) = get(
        &fixture.router,
        "/api/projects/p1/chat-artifact-snapshots/s1",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(no_store(&headers));
    assert_eq!(content_type(&headers), "application/json");

    let snapshot = &json(&body)["snapshot"];
    assert_eq!(snapshot["id"], "s1");
    assert_eq!(snapshot["projectId"], "p1");
    assert_eq!(snapshot["sourcePathAtCapture"], "Design Files/hero image.png");
    assert_eq!(snapshot["kind"], "image");
    assert_eq!(snapshot["state"], "ready");
    assert_eq!(snapshot["createdAt"], 1_700_000_000_000_i64);
    assert_eq!(snapshot["readyAt"], 1_700_000_000_500_i64);
    assert_eq!(snapshot["workspaceArtifactId"], "wa1");
    assert_eq!(snapshot["mime"], "image/png");
    assert_eq!(snapshot["contentDigest"], content_digest());
    assert_eq!(snapshot["thumbnailDigest"], thumb_digest());
    assert_eq!(snapshot["runId"], "run-1");
    assert_eq!(snapshot["mediaTaskId"], "task-1");
    assert_eq!(snapshot["byteSize"], PNG_BYTES.len() as i64);
    assert!(snapshot.get("failureCode").is_none(), "{snapshot}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_routes_404_across_projects_and_unknown_ids() {
    let fixture = setup("snapshot-404");
    seed_chat_artifacts(&fixture);

    for uri in [
        "/api/projects/p1/chat-artifact-snapshots/ghost",
        "/api/projects/p2/chat-artifact-snapshots/s1",
        "/api/projects/p1/chat-artifact-snapshots/ghost/content",
        "/api/projects/p2/chat-artifact-snapshots/s1/thumbnail",
    ] {
        let (status, _, body) = get(&fixture.router, uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}: {body:?}");
        assert_eq!(json(&body)["error"]["code"], "ARTIFACT_NOT_FOUND", "{uri}");
        assert_eq!(json(&body)["error"]["message"], "snapshot not found", "{uri}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_content_serves_verified_bytes_with_etag_and_304() {
    let fixture = setup("content-happy");
    seed_chat_artifacts(&fixture);

    let (status, headers, body) = get(
        &fixture.router,
        "/api/projects/p1/chat-artifact-snapshots/s1/content",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, PNG_BYTES);
    let etag = format!("\"{}\"", content_digest());
    assert_eq!(header_value(&headers, header::ETAG), Some(etag.as_str()));
    assert_eq!(
        header_value(&headers, header::CACHE_CONTROL),
        Some("private, max-age=31536000, immutable")
    );
    assert_eq!(
        header_value(&headers, header::X_CONTENT_TYPE_OPTIONS),
        Some("nosniff")
    );
    assert_eq!(content_type(&headers), "image/png");
    assert!(
        header_value(&headers, header::CONTENT_DISPOSITION).is_none(),
        "an inline-safe type must not be forced to an attachment"
    );

    // Conditional request: exact-string If-None-Match → 304, no body.
    let (status, headers, body) = get_with(
        &fixture.router,
        "/api/projects/p1/chat-artifact-snapshots/s1/content",
        &[(header::IF_NONE_MATCH, etag.as_str())],
        Some(loopback()),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert!(body.is_empty(), "{body:?}");
    assert_eq!(header_value(&headers, header::ETAG), Some(etag.as_str()));
    assert_eq!(
        header_value(&headers, header::CACHE_CONTROL),
        Some("private, max-age=31536000, immutable")
    );

    // A stale validator is answered with the bytes again.
    let (status, _, body) = get_with(
        &fixture.router,
        "/api/projects/p1/chat-artifact-snapshots/s1/content",
        &[(header::IF_NONE_MATCH, "\"stale\"")],
        Some(loopback()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, PNG_BYTES);

    // The thumbnail half is served from its own digest.
    let (status, headers, body) = get(
        &fixture.router,
        "/api/projects/p1/chat-artifact-snapshots/s1/thumbnail",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, THUMB_BYTES);
    assert_eq!(
        header_value(&headers, header::ETAG),
        Some(format!("\"{}\"", thumb_digest()).as_str())
    );
    assert_eq!(content_type(&headers), "image/png");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_content_forces_attachment_for_non_inline_mime() {
    let fixture = setup("content-attachment");
    seed_chat_artifacts(&fixture);

    let (status, headers, body) = get(
        &fixture.router,
        "/api/projects/p1/chat-artifact-snapshots/s6/content",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, SVG_BYTES);
    assert_eq!(content_type(&headers), "application/octet-stream");
    assert_eq!(
        header_value(&headers, header::CONTENT_DISPOSITION),
        Some("attachment; filename=\"pic.svg\"")
    );
    assert_eq!(
        header_value(&headers, header::X_CONTENT_TYPE_OPTIONS),
        Some("nosniff")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_content_explains_pending_and_missing_blobs() {
    let fixture = setup("content-404");
    seed_chat_artifacts(&fixture);

    let (status, _, body) = get(
        &fixture.router,
        "/api/projects/p1/chat-artifact-snapshots/s2/content",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let error = &json(&body)["error"];
    assert_eq!(error["code"], "ARTIFACT_NOT_FOUND");
    assert_eq!(error["message"], "snapshot content is not available");
    assert_eq!(error["details"]["state"], "pending");
    assert_eq!(error["details"]["failureCode"], "source_missing");

    // Ready state, digest, but no blob row: the same message, no details.
    let (status, _, body) = get(
        &fixture.router,
        "/api/projects/p1/chat-artifact-snapshots/s3/content",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let error = &json(&body)["error"];
    assert_eq!(error["message"], "snapshot content is not available");
    assert!(error.get("details").is_none(), "{error}");

    // The thumbnail half of a pending snapshot reports the same state.
    let (status, _, body) = get(
        &fixture.router,
        "/api/projects/p1/chat-artifact-snapshots/s2/thumbnail",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json(&body)["error"]["details"]["state"], "pending");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_content_fails_verification_with_410() {
    let fixture = setup("content-410");
    seed_chat_artifacts(&fixture);

    for uri in [
        // The row's byte count disagrees with the file on disk.
        "/api/projects/p1/chat-artifact-snapshots/s4/content",
        // The row's storage key cannot resolve under the blob root.
        "/api/projects/p1/chat-artifact-snapshots/s5/content",
    ] {
        let (status, _, body) = get(&fixture.router, uri).await;
        assert_eq!(status, StatusCode::GONE, "{uri}: {body:?}");
        let error = &json(&body)["error"];
        assert_eq!(error["code"], "ARTIFACT_NOT_FOUND", "{uri}");
        assert_eq!(
            error["message"],
            "snapshot content failed verification",
            "{uri}"
        );
        assert_eq!(error["details"]["reason"], "blob_verification_failed", "{uri}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_artifact_metadata_and_tombstones() {
    let fixture = setup("workspace-artifact");
    seed_chat_artifacts(&fixture);

    let (status, headers, body) = get(
        &fixture.router,
        "/api/projects/p1/workspace-artifacts/wa1",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(no_store(&headers));
    let artifact = &json(&body)["artifact"];
    assert_eq!(artifact["id"], "wa1");
    assert_eq!(artifact["projectId"], "p1");
    assert_eq!(artifact["currentPath"], "Design Files/hero.png");
    assert_eq!(artifact["kind"], "image");
    assert_eq!(artifact["mime"], "image/png");
    assert_eq!(artifact["currentDigest"], content_digest());
    assert_eq!(artifact["currentSize"], PNG_BYTES.len() as i64);
    assert_eq!(artifact["currentMtime"], 1_700_000_100_000_i64);
    assert_eq!(artifact["deleted"], false);
    assert_eq!(artifact["createdAt"], 1);
    assert_eq!(artifact["updatedAt"], 2);

    let (status, _, body) = get(
        &fixture.router,
        "/api/projects/p1/workspace-artifacts/wt1",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tombstone = &json(&body)["artifact"];
    assert_eq!(tombstone["deleted"], true);
    assert!(tombstone["currentPath"].is_null(), "{tombstone}");
    assert!(tombstone.get("mime").is_none(), "{tombstone}");
    assert!(tombstone.get("currentDigest").is_none(), "{tombstone}");
    assert!(tombstone.get("currentSize").is_none(), "{tombstone}");

    for uri in [
        "/api/projects/p1/workspace-artifacts/ghost",
        "/api/projects/p2/workspace-artifacts/wa1",
    ] {
        let (status, _, body) = get(&fixture.router, uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}: {body:?}");
        assert_eq!(json(&body)["error"]["code"], "ARTIFACT_NOT_FOUND", "{uri}");
        assert_eq!(json(&body)["error"]["message"], "artifact not found", "{uri}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn artifact_routes_gate_on_project_and_revocation() {
    let fixture = setup("project-gate");
    seed_chat_artifacts(&fixture);

    for uri in [
        "/api/projects/ghost/chat-artifact-snapshots/s1",
        "/api/projects/ghost/workspace-artifacts/wa1",
        "/api/projects/ghost/conversations/c1/messages/m1/artifacts",
        // The team-mirror revocation check answers exactly like a missing row.
        "/api/projects/p-revoked/chat-artifact-snapshots/s1",
        "/api/projects/p-revoked/workspace-artifacts/wa1",
        "/api/projects/p-revoked/conversations/c1/messages/m1/artifacts",
    ] {
        let (status, _, body) = get(&fixture.router, uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}: {body:?}");
        assert_eq!(json(&body)["error"]["code"], "PROJECT_NOT_FOUND", "{uri}");
        assert_eq!(json(&body)["error"]["message"], "project not found", "{uri}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn artifact_routes_enforce_api_token_auth() {
    let fixture = setup_with_token("auth", Some("s3cret"));
    seed_skill(
        &fixture.data_root,
        "demo",
        Some("demo-skill"),
        &[("example.html", "AUTH-EXAMPLE")],
    );
    let uri = "/api/skills/demo-skill/example";

    // No loopback peer and no credentials → 401 (fail closed).
    let (status, headers, body) = get_with(&fixture.router, uri, &[], None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(json(&body)["error"]["code"], "UNAUTHORIZED");
    assert!(
        header_value(&headers, header::WWW_AUTHENTICATE).is_some(),
        "the 401 must carry a challenge"
    );

    // Exact bearer credentials from a non-loopback peer → allowed.
    let (status, _, body) = get_with(
        &fixture.router,
        uri,
        &[(header::AUTHORIZATION, "Bearer s3cret")],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));

    // Loopback peers skip credentials (parity: the desktop UI).
    let (status, _, body) = get_with(&fixture.router, uri, &[], Some(loopback())).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));

    // A wrong token is still denied.
    let (status, _, _) = get_with(
        &fixture.router,
        uri,
        &[(header::AUTHORIZATION, "Bearer wrong")],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
