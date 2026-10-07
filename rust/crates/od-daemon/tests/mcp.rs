//! Integration tests: `GET`/`PUT /api/mcp/servers` over a real socket —
//! TypeScript parity for the routes at `apps/daemon/src/mcp-routes.ts:191`
//! / `:205` and the storage layer in `apps/daemon/src/mcp-config.ts`.

use std::net::SocketAddr;
use std::path::Path;

use od_core::RuntimePaths;
use od_daemon::{DaemonConfig, RunningDaemon};
use serde_json::{json, Value};

fn temp_paths(tag: &str) -> RuntimePaths {
    let dir = std::env::temp_dir().join(format!("od-daemon-mcp-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    RuntimePaths::resolve(dir).expect("resolve data dir")
}

fn config(paths: RuntimePaths) -> DaemonConfig {
    DaemonConfig {
        paths,
        bind_host: "127.0.0.1".to_string(),
        port: 0,
        api_token: None,
        api_auth_disabled: false,
        web_dist: None,
    }
}

async fn send(addr: SocketAddr, request: &str) -> (String, String) {
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    tokio::io::AsyncWriteExt::write_all(&mut stream, request.as_bytes())
        .await
        .expect("write");
    let mut response = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response)
        .await
        .expect("read");
    let response = String::from_utf8_lossy(&response).into_owned();
    let (head, body) = response.split_once("\r\n\r\n").unwrap_or(("", ""));
    (head.to_string(), body.to_string())
}

async fn get_servers(addr: SocketAddr, origin: Option<&str>) -> (String, String) {
    let origin = origin
        .map(|value| format!("Origin: {value}\r\n"))
        .unwrap_or_default();
    send(
        addr,
        &format!(
            "GET /api/mcp/servers HTTP/1.1\r\nHost: {addr}\r\n{origin}Connection: close\r\n\r\n"
        ),
    )
    .await
}

async fn put_servers(addr: SocketAddr, body: &str, origin: Option<&str>) -> (String, String) {
    let origin = origin
        .map(|value| format!("Origin: {value}\r\n"))
        .unwrap_or_default();
    send(
        addr,
        &format!(
            "PUT /api/mcp/servers HTTP/1.1\r\nHost: {addr}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\
             {origin}Connection: close\r\n\r\n{body}",
            body.len()
        ),
    )
    .await
}

fn response_json(body: &str) -> Value {
    serde_json::from_str(body).expect("response body is JSON")
}

const STDIO_BODY: &str = r#"{"servers":[{"id":"github","label":" GitHub ","transport":"stdio","command":" npx ","args":["-y","@modelcontextprotocol/server-github"],"env":{"GITHUB_PERSONAL_ACCESS_TOKEN":"ghp_xxx"},"url":"https://ignored.example/","authMode":"oauth","headers":{"H":"v"}}]}"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_without_a_config_file_returns_empty_servers_and_all_templates() {
    let paths = temp_paths("empty");
    let data_dir = paths.data_dir().to_path_buf();
    let daemon = RunningDaemon::start(config(paths)).await.expect("start");
    let addr = daemon.addr();

    let (head, body) = get_servers(addr, None).await;
    assert!(head.contains("200 OK"), "status: {head}");
    // Compact body, key order identical to TS
    // `res.json({ servers: cfg.servers, templates: MCP_TEMPLATES })`.
    assert!(
        body.starts_with(r#"{"servers":[],"templates":["#),
        "body: {body}"
    );
    let parsed = response_json(&body);
    assert_eq!(parsed["servers"], json!([]));
    let templates = parsed["templates"].as_array().expect("templates array");
    assert!(!templates.is_empty(), "the built-in template list ships");
    assert!(templates[0].get("id").and_then(Value::as_str).is_some());

    // Reads never create the config file (parity: `readMcpConfig` only
    // touches the disk when the file already exists).
    assert!(
        !data_dir.join("mcp-config.json").exists(),
        "GET must not create mcp-config.json"
    );
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn put_persists_a_stdio_server_and_get_reads_it_back() {
    let paths = temp_paths("put");
    let data_dir = paths.data_dir().to_path_buf();
    let daemon = RunningDaemon::start(config(paths)).await.expect("start");
    let addr = daemon.addr();

    let (head, body) = put_servers(addr, STDIO_BODY, None).await;
    assert!(head.contains("200 OK"), "status: {head}");
    // The sanitized echo: trimmed label/command, invalid url/authMode/
    // headers dropped from a stdio entry (they never appear in the JSON),
    // fields in TS insertion order.
    let expected_prefix = concat!(
        r#"{"servers":[{"id":"github","transport":"stdio","enabled":true,"label":"GitHub","#,
        r#""command":"npx","args":["-y","@modelcontextprotocol/server-github"],"#,
        r#""env":{"GITHUB_PERSONAL_ACCESS_TOKEN":"ghp_xxx"}}],"templates":["#,
    );
    assert!(body.starts_with(expected_prefix), "echo: {body}");
    let echoed = response_json(&body);
    assert!(echoed["servers"][0].get("url").is_none());
    assert!(echoed["servers"][0].get("authMode").is_none());
    assert!(echoed["servers"][0].get("headers").is_none());

    // GET returns the same sanitized entry.
    let (head, body) = get_servers(addr, None).await;
    assert!(head.contains("200 OK"), "status: {head}");
    let fetched = response_json(&body);
    assert_eq!(fetched["servers"], echoed["servers"]);
    assert_eq!(fetched["templates"], echoed["templates"]);

    // The file exists, parses, matches the response, and its raw text is
    // the 2-space pretty serialization (`JSON.stringify(next, null, 2)`).
    let file = data_dir.join("mcp-config.json");
    let text = std::fs::read_to_string(&file).expect("config file on disk");
    assert!(
        text.starts_with("{\n  \"servers\": [\n    {\n      \"id\": \"github\","),
        "raw file: {text}"
    );
    assert!(!text.ends_with('\n'), "no trailing newline, like JSON.stringify");
    let on_disk: Value = serde_json::from_str(&text).expect("file is valid JSON");
    assert_eq!(on_disk["servers"], echoed["servers"]);
    assert_eq!(text, serde_json::to_string_pretty(&on_disk).expect("pretty"));

    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn put_keeps_only_the_valid_entries() {
    let paths = temp_paths("mixed");
    let data_dir = paths.data_dir().to_path_buf();
    let daemon = RunningDaemon::start(config(paths)).await.expect("start");
    let addr = daemon.addr();

    let body = r#"{"servers":[
        {"id":"bad","transport":"stdio"},
        {"id":"NOT VALID id","command":"echo"},
        {"id":"../evil","command":"echo"},
        {"id":"good","transport":"stdio","command":"echo","enabled":false},
        {"id":"good","transport":"stdio","command":"other"}
    ]}"#;
    let (head, response) = put_servers(addr, body, None).await;
    assert!(head.contains("200 OK"), "status: {head}");
    let echoed = response_json(&response);
    let ids: Vec<&str> = echoed["servers"]
        .as_array()
        .expect("servers array")
        .iter()
        .map(|server| server["id"].as_str().expect("id"))
        .collect();
    assert_eq!(ids, ["good"], "invalid entries dropped, first id wins");
    // First occurrence wins for the rest of the entry too, and a literal
    // `enabled: false` survives sanitization.
    assert_eq!(echoed["servers"][0]["command"], json!("echo"));
    assert_eq!(echoed["servers"][0]["enabled"], json!(false));

    // The same entry list comes back from GET …
    let (_, body) = get_servers(addr, None).await;
    assert_eq!(response_json(&body)["servers"], echoed["servers"]);
    // … and from the persisted file.
    let text = std::fs::read_to_string(data_dir.join("mcp-config.json")).expect("file");
    let on_disk: Value = serde_json::from_str(&text).expect("file JSON");
    assert_eq!(on_disk["servers"], echoed["servers"]);
    assert_eq!(text, serde_json::to_string_pretty(&on_disk).expect("pretty"));

    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_origin_requests_are_rejected_before_any_write() {
    let paths = temp_paths("cross");
    let data_dir = paths.data_dir().to_path_buf();
    let daemon = RunningDaemon::start(config(paths)).await.expect("start");
    let addr = daemon.addr();

    let (head, body) = put_servers(addr, STDIO_BODY, Some("http://evil.example")).await;
    assert!(head.contains("403 Forbidden"), "status: {head}");
    // The crate-wide envelope carries the TS message verbatim (see the
    // DOCUMENTED DEVIATIONS in mcp.rs).
    assert!(body.contains(r#""code":"FORBIDDEN""#), "envelope: {body}");
    assert!(
        body.contains("cross-origin request rejected"),
        "message parity: {body}"
    );
    assert!(
        !data_dir.join("mcp-config.json").exists(),
        "the guard must run before writeMcpConfig"
    );

    // The read side refuses the same way.
    let (head, body) = get_servers(addr, Some("http://evil.example")).await;
    assert!(head.contains("403 Forbidden"), "status: {head}");
    assert!(body.contains("cross-origin request rejected"), "body: {body}");

    // A same-origin request afterwards still works — the rejection is not
    // sticky.
    let (head, _) = put_servers(addr, STDIO_BODY, None).await;
    assert!(head.contains("200 OK"), "status: {head}");
    assert!(data_dir.join("mcp-config.json").exists());

    daemon.shutdown().await;
}

/// The template ids embedded in `mcp-templates.json` must stay in lockstep
/// with the `MCP_TEMPLATES` literal in the TypeScript source — the JSON is
/// generated from it, and this test fails loudly when either side drifts.
#[test]
fn template_ids_match_the_typescript_literal() {
    let ts_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../apps/daemon/src/mcp-config.ts");
    let ts = match std::fs::read_to_string(&ts_path) {
        Ok(ts) => ts,
        Err(err) => {
            eprintln!("skipping template-id parity, cannot read {}: {err}", ts_path.display());
            return;
        }
    };
    let marker = ts.find("export const MCP_TEMPLATES").expect("MCP_TEMPLATES marker");
    // The array literal is the last declaration in the file; take up to its
    // final `];` (inner arrays close with `],`).
    let end = ts[marker..].rfind("];").map(|offset| marker + offset + 2).expect("closing ];");
    let span = &ts[marker..end];

    let mut ts_ids = Vec::new();
    for line in span.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("id: ") else {
            continue;
        };
        let Some(quote) = rest.chars().next().filter(|c| *c == '\'' || *c == '"') else {
            continue;
        };
        let body = &rest[quote.len_utf8()..];
        let Some(close) = body.find(quote) else {
            continue;
        };
        ts_ids.push(body[..close].to_string());
    }
    assert!(!ts_ids.is_empty(), "extracted no template ids from the TS span");

    let json_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("mcp-templates.json");
    let json = std::fs::read_to_string(&json_path).expect("mcp-templates.json");
    let parsed: Value = serde_json::from_str(&json).expect("mcp-templates.json parses");
    let json_ids: Vec<&str> = parsed
        .as_array()
        .expect("template array")
        .iter()
        .map(|template| template.get("id").and_then(Value::as_str).expect("template id"))
        .collect();

    assert_eq!(ts_ids, json_ids, "mcp-templates.json is stale — regenerate it from the TS literal");
}
