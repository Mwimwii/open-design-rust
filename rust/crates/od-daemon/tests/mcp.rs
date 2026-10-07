//! Integration tests: the MCP routes over a real socket — `GET`/`PUT
//! /api/mcp/servers (parity for the routes at
//! `apps/daemon/src/mcp-routes.ts:191` / `:205` and the storage layer in
//! `apps/daemon/src/mcp-config.ts`), plus the step-3 install surface:
//! `GET /api/mcp/install-info` and the three Codex one-click install routes
//! (mcp-routes.ts:96-185).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

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

async fn get_install_info(addr: SocketAddr, origin: Option<&str>) -> (String, String) {
    let origin = origin
        .map(|value| format!("Origin: {value}\r\n"))
        .unwrap_or_default();
    send(
        addr,
        &format!(
            "GET /api/mcp/install-info HTTP/1.1\r\nHost: {addr}\r\n{origin}Connection: close\r\n\r\n"
        ),
    )
    .await
}

async fn get_codex_status(addr: SocketAddr) -> (String, String) {
    send(
        addr,
        &format!(
            "GET /api/mcp/install/codex/status HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
        ),
    )
    .await
}

/// The install routes take no request body (parity: the TS handlers never
/// read `req.body`), so the request carries an explicit empty one.
async fn post_codex_install(addr: SocketAddr) -> (String, String) {
    send(
        addr,
        &format!(
            "POST /api/mcp/install/codex HTTP/1.1\r\nHost: {addr}\r\n\
             Content-Length: 0\r\nConnection: close\r\n\r\n"
        ),
    )
    .await
}

async fn delete_codex_install(addr: SocketAddr) -> (String, String) {
    send(
        addr,
        &format!(
            "DELETE /api/mcp/install/codex HTTP/1.1\r\nHost: {addr}\r\n\
             Content-Length: 0\r\nConnection: close\r\n\r\n"
        ),
    )
    .await
}

fn response_json(body: &str) -> Value {
    serde_json::from_str(body).expect("response body is JSON")
}

/// The `{error:{code,message}}` envelope every failure body uses (see the
/// DOCUMENTED DEVIATIONS in `mcp.rs`); returns `(code, message)`.
fn error_envelope(body: &str) -> (String, String) {
    let error = response_json(body)
        .get("error")
        .cloned()
        .expect("failure body carries the envelope");
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .expect("error.code")
        .to_string();
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .expect("error.message")
        .to_string();
    (code, message)
}

// ---- process-env isolation for the step-3 install routes ----------------

/// The vars the install payload and its cache read from the process env.
/// Every test that reads or mutates them serializes on [`ENV_LOCK`] and
/// restores the original values on drop (same pattern as `tests/sandbox.rs`).
const MCP_ENV_VARS: &[&str] = &[
    "OD_WEB_PORT",
    "OD_BIN",
    "OD_DAEMON_CLI_PATH",
    "OD_MCP_BOOTSTRAP_COMMAND",
    "OD_MCP_BOOTSTRAP_ARGS",
];

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    saved: Vec<(String, Option<String>)>,
}

impl EnvGuard {
    /// Hold the env lock for the rest of the test: every var in
    /// [`MCP_ENV_VARS`] plus `extra_vars` is cleared (its old value
    /// remembered) so assertions do not depend on the ambient environment.
    fn acquire(extra_vars: &[&str]) -> Self {
        let lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut guard = EnvGuard {
            _lock: lock,
            saved: Vec::new(),
        };
        for var in MCP_ENV_VARS.iter().chain(extra_vars) {
            guard.clear(var);
        }
        guard
    }

    fn clear(&mut self, var: &str) {
        self.save(var);
        std::env::remove_var(var);
    }

    /// Save `var` once and set it — [`Drop`] puts the original back.
    fn set(&mut self, var: &str, value: &str) {
        self.save(var);
        std::env::set_var(var, value);
    }

    fn save(&mut self, var: &str) {
        if !self.saved.iter().any(|(name, _)| name == var) {
            self.saved.push((var.to_string(), std::env::var(var).ok()));
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (var, value) in self.saved.drain(..) {
            match value {
                Some(value) => std::env::set_var(var, value),
                None => std::env::remove_var(var),
            }
        }
    }
}

/// Find `codex` the way the runner would (a `PATH` search), so the status
/// assertions can branch on what this machine actually has.
fn codex_on_path() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let names: &[&str] = if cfg!(windows) {
        &["codex.cmd", "codex.exe", "codex"]
    } else {
        &["codex"]
    };
    std::env::split_paths(&path)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
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

// ---- step 3: install-info + Codex one-click install routes --------------

/// `GET /api/mcp/install-info` (parity: mcp-routes.ts:96-112) over a real
/// socket: the payload must describe *this* running daemon — bound port,
/// data dir, platform — and keep the TS key order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_info_reports_the_bound_port_data_dir_and_platform() {
    let _guard = EnvGuard::acquire(&[]);
    let paths = temp_paths("install-info");
    let data_dir = paths.data_dir().to_path_buf();
    let daemon = RunningDaemon::start(config(paths)).await.expect("start");
    let addr = daemon.addr();

    let (head, body) = get_install_info(addr, None).await;
    assert!(head.contains("200 OK"), "status: {head} body: {body}");
    let payload = response_json(&body);

    // Key order matches the TS return literal (mcp-install-info.ts:94-110).
    let keys: Vec<&str> = payload
        .as_object()
        .expect("payload object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "command",
            "args",
            "env",
            "daemonUrl",
            "webBaseUrl",
            "platform",
            "cliExists",
            "nodeExists",
            "buildHint",
        ],
        "body: {body}"
    );

    let daemon_url = format!("http://127.0.0.1:{}", addr.port());
    assert_eq!(payload["daemonUrl"], json!(daemon_url));
    // `command` is TS's `process.execPath` — this daemon runs in-process in
    // the test binary, so the payload must name exactly that.
    let exe = std::env::current_exe()
        .expect("current exe")
        .to_string_lossy()
        .into_owned();
    assert_eq!(payload["command"], json!(exe));
    let expected_platform = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    };
    assert_eq!(payload["platform"], json!(expected_platform));
    assert_eq!(
        payload["env"]["OD_DATA_DIR"],
        json!(data_dir.display().to_string())
    );

    // Non-sidecar form: the args end in `--daemon-url <bound url>`.
    let args = payload["args"].as_array().expect("args array");
    assert_eq!(args.len(), 4, "args: {args:?}");
    assert_eq!(args[1], json!("mcp"));
    assert_eq!(args[2], json!("--daemon-url"));
    assert_eq!(args[3], json!(daemon_url));

    // `nodeExists` probes the running binary (always true here), so
    // `buildHint` is present exactly when the `od` CLI entry is missing —
    // and it names that entry (args[0]).
    assert_eq!(payload["nodeExists"], json!(true));
    assert_eq!(
        payload["buildHint"].is_null(),
        payload["cliExists"].as_bool().expect("cliExists bool"),
        "buildHint mirrors cliExists: {body}"
    );
    if let Some(hint) = payload["buildHint"].as_str() {
        assert!(
            hint.starts_with("OpenDesign CLI entry is missing at "),
            "hint: {hint}"
        );
        assert!(
            hint.contains(args[0].as_str().expect("cli path")),
            "hint: {hint}"
        );
    }

    // OD_WEB_PORT was cleared for this test → no studio deep link.
    assert_eq!(payload["webBaseUrl"], Value::Null);

    daemon.shutdown().await;
}

/// The 5s TTL cache (mcp-routes.ts:100-111) serves byte-identical payloads
/// within the window, recomputes when its `OD_WEB_PORT` key changes, and
/// never gets ahead of the same-origin guard.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_info_caches_within_the_ttl_and_still_rejects_cross_origin() {
    let mut guard = EnvGuard::acquire(&[]);
    let paths = temp_paths("install-cache");
    let daemon = RunningDaemon::start(config(paths)).await.expect("start");
    let addr = daemon.addr();

    let (head, first) = get_install_info(addr, None).await;
    assert!(head.contains("200 OK"), "status: {head} body: {first}");

    // A recompute would fold the bootstrap env into `env`; a cache hit
    // cannot — identical bytes inside the TTL are the proof of the hit.
    guard.set("OD_MCP_BOOTSTRAP_COMMAND", "od mcp bootstrap");
    let (head, second) = get_install_info(addr, None).await;
    assert!(head.contains("200 OK"), "status: {head} body: {second}");
    assert_eq!(first, second, "the 5s TTL must serve the cached payload");
    guard.clear("OD_MCP_BOOTSTRAP_COMMAND");

    // The cache key is the raw OD_WEB_PORT value: a change forces a
    // recompute, which surfaces the studio deep link.
    guard.set("OD_WEB_PORT", "65321");
    let (head, third) = get_install_info(addr, None).await;
    assert!(head.contains("200 OK"), "status: {head} body: {third}");
    assert_ne!(first, third, "a new web port must invalidate the cache");
    assert_eq!(
        response_json(&third)["webBaseUrl"],
        json!("http://127.0.0.1:65321"),
        "body: {third}"
    );

    // The guard runs before the cache, exactly as in TS.
    let (head, body) = get_install_info(addr, Some("http://evil.example")).await;
    assert!(head.contains("403 Forbidden"), "status: {head}");
    assert!(
        body.contains("cross-origin request rejected"),
        "body: {body}"
    );

    daemon.shutdown().await;
}

/// `GET /api/mcp/install/codex/status` (parity: mcp-routes.ts:142-152):
/// 200 with `{available, installed}` — the "no Codex CLI" case is a valid
/// answer, not an error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_status_answers_200_with_whether_this_machine_has_the_cli() {
    let _guard = EnvGuard::acquire(&[]);
    let paths = temp_paths("codex-status");
    let daemon = RunningDaemon::start(config(paths)).await.expect("start");
    let addr = daemon.addr();

    let (head, body) = get_codex_status(addr).await;
    assert!(head.contains("200 OK"), "status: {head} body: {body}");
    let status = response_json(&body);
    let available = status["available"].as_bool().expect("available is a bool");
    let installed = status["installed"].as_bool().expect("installed is a bool");
    match codex_on_path() {
        // Codex present → the probe ran; `installed` reflects this box's
        // `~/.codex/config.toml`, so it is not pinned either way.
        Some(_) => assert!(available, "codex is on PATH: {body}"),
        // The likely case: no CLI → both flags false, never a 500.
        None => {
            assert_eq!(status["available"], json!(false), "body: {body}");
            assert_eq!(status["installed"], json!(false), "body: {body}");
        }
    }
    let _ = installed;

    daemon.shutdown().await;
}

/// `POST`/`DELETE /api/mcp/install/codex` (parity: mcp-routes.ts:154-185):
/// the refusal, install-failure, and uninstall-failure shapes, each with its
/// TS envelope code and a non-empty message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_install_routes_answer_with_their_envelope_codes() {
    // PATH joins the cleared vars: an empty dir on PATH makes the runner
    // deterministically report "no codex" whatever this machine has, so the
    // tests never rewrite a real `~/.codex/config.toml`.
    let mut guard = EnvGuard::acquire(&["PATH"]);
    let paths = temp_paths("codex-install");
    let daemon = RunningDaemon::start(config(paths)).await.expect("start");
    let addr = daemon.addr();

    // Missing CLI entry → the payload refuses before codex is ever spawned;
    // the message is the buildHint (TS `payload.buildHint ?? …`).
    let missing_cli =
        std::env::temp_dir().join(format!("od-mcp-missing-cli-{}", std::process::id()));
    let missing = missing_cli.to_string_lossy().into_owned();
    guard.set("OD_BIN", &missing);
    let (head, body) = post_codex_install(addr).await;
    assert!(
        head.contains("500 Internal Server Error"),
        "status: {head} body: {body}"
    );
    let (code, message) = error_envelope(&body);
    assert_eq!(code, "INSTALL_INFO_INCOMPLETE", "body: {body}");
    assert!(
        message.starts_with("OpenDesign CLI entry is missing at "),
        "message: {message}"
    );
    assert!(message.contains("od-mcp-missing-cli"), "message: {message}");

    // Complete payload + no reachable codex → the runner error surfaces as
    // CODEX_INSTALL_FAILED (TS `String(err.message)`).
    let empty_bin = std::env::temp_dir().join(format!("od-mcp-empty-bin-{}", std::process::id()));
    std::fs::create_dir_all(&empty_bin).expect("empty bin dir");
    guard.set("PATH", empty_bin.to_string_lossy().as_ref());
    let exe = std::env::current_exe()
        .expect("current exe")
        .to_string_lossy()
        .into_owned();
    guard.set("OD_BIN", &exe);
    let (head, body) = post_codex_install(addr).await;
    assert!(
        head.contains("500 Internal Server Error"),
        "status: {head} body: {body}"
    );
    let (code, message) = error_envelope(&body);
    assert_eq!(code, "CODEX_INSTALL_FAILED", "body: {body}");
    assert!(!message.is_empty(), "message must not be empty");
    assert!(message.contains("codex"), "message: {message}");

    // DELETE runs `codex mcp remove` unconditionally → the same shape with
    // its own code (the `{"ok":true}` success branch needs a real codex).
    let (head, body) = delete_codex_install(addr).await;
    assert!(
        head.contains("500 Internal Server Error"),
        "status: {head} body: {body}"
    );
    let (code, message) = error_envelope(&body);
    assert_eq!(code, "CODEX_UNINSTALL_FAILED", "body: {body}");
    assert!(!message.is_empty(), "message must not be empty");
    assert!(message.contains("codex"), "message: {message}");

    let _ = std::fs::remove_dir_all(&empty_bin);
    daemon.shutdown().await;
}
