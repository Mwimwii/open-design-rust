//! External MCP server injection at spawn — Rust parity port of the spawn
//! side of `apps/daemon/src/mcp-config.ts` (`isManagedProjectCwd`,
//! `buildClaudeMcpJson`, `buildOpenCodeMcpConfigContent`) as consumed by
//! the run spawn pipeline.
//!
//! Strategies follow the def's `externalMcpInjection` field:
//! `claude-mcp-json` writes `.mcp.json` into a managed project cwd;
//! `opencode-env-content` / `mimo-env-content` produce the inline config
//! for `OPENCODE_CONFIG_CONTENT` / `MIMOCODE_CONFIG_CONTENT`. `acp-merge`
//! needs the ACP session layer (protocol bead) and is not handled here.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::mcp::{effective_mcp_auth_mode, McpAuthMode, McpServerConfig, McpTransport};

/// Parity: `isManagedProjectCwd` (mcp-config.ts:285) — only a managed
/// project directory may receive a written `.mcp.json`.
pub fn is_managed_project_cwd(cwd: Option<&str>, projects_dir: &Path) -> bool {
    let Some(cwd) = cwd.filter(|cwd| !cwd.is_empty()) else {
        return false;
    };
    if projects_dir.as_os_str().is_empty() {
        return false;
    }
    if Path::new(cwd) == projects_dir {
        return false;
    }
    Path::new(cwd).starts_with(projects_dir)
}

fn string_map(map: Option<&Map<String, Value>>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Some(map) = map {
        for (key, value) in map {
            if let Some(text) = value.as_str() {
                out.insert(key.clone(), text.to_string());
            }
        }
    }
    out
}

/// Parity: `mergeAuthHeader` (mcp-config.ts:348) — user-pinned
/// non-empty `Authorization` wins; blank ones never suppress the daemon
/// Bearer; otherwise the daemon token is injected.
fn merge_auth_header(
    existing: &HashMap<String, String>,
    bearer: Option<&str>,
) -> Option<HashMap<String, String>> {
    let mut merged = HashMap::new();
    let mut user_auth: Option<String> = None;
    for (key, value) in existing {
        if key.eq_ignore_ascii_case("authorization") {
            if !value.trim().is_empty() {
                user_auth = Some(value.clone());
                merged.insert(key.clone(), value.clone());
            }
            continue;
        }
        merged.insert(key.clone(), value.clone());
    }
    if let Some(token) = bearer.filter(|token| !token.is_empty()) {
        if user_auth.is_none() {
            merged.insert("Authorization".to_string(), format!("Bearer {token}"));
        }
    }
    if merged.is_empty() {
        return None;
    }
    Some(merged)
}

fn oauth_bearer(
    server: &McpServerConfig,
    tokens: &HashMap<String, String>,
) -> Option<String> {
    if effective_mcp_auth_mode(server) != McpAuthMode::Oauth {
        return None;
    }
    tokens.get(&server.id).cloned()
}

/// Parity: `buildClaudeMcpJson` (mcp-config.ts:312). `None` when no
/// server is enabled — the caller must not write the file then (and
/// should remove a stale one).
pub fn build_claude_mcp_json(
    servers: &[McpServerConfig],
    tokens: &HashMap<String, String>,
) -> Option<Value> {
    let mut out = Map::new();
    for server in servers.iter().filter(|s| s.enabled) {
        match server.transport {
            McpTransport::Stdio => {
                let mut entry = Map::new();
                if let Some(command) = server.command.as_deref() {
                    entry.insert("command".to_string(), Value::String(command.to_string()));
                }
                if let Some(args) = server.args.as_ref().filter(|args| !args.is_empty()) {
                    entry.insert(
                        "args".to_string(),
                        Value::Array(args.iter().map(|a| Value::String(a.clone())).collect()),
                    );
                }
                let env = string_map(server.env.as_ref());
                if !env.is_empty() {
                    entry.insert(
                        "env".to_string(),
                        Value::Object(
                            env.into_iter()
                                .map(|(k, v)| (k, Value::String(v)))
                                .collect(),
                        ),
                    );
                }
                out.insert(server.id.clone(), Value::Object(entry));
            }
            McpTransport::Sse | McpTransport::Http => {
                let mut entry = Map::new();
                entry.insert(
                    "type".to_string(),
                    Value::String(
                        if server.transport == McpTransport::Sse {
                            "sse"
                        } else {
                            "http"
                        }
                        .to_string(),
                    ),
                );
                if let Some(url) = server.url.as_deref() {
                    entry.insert("url".to_string(), Value::String(url.to_string()));
                }
                let headers = string_map(server.headers.as_ref());
                if let Some(merged) =
                    merge_auth_header(&headers, oauth_bearer(server, tokens).as_deref())
                {
                    entry.insert(
                        "headers".to_string(),
                        Value::Object(
                            merged
                                .into_iter()
                                .map(|(k, v)| (k, Value::String(v)))
                                .collect(),
                        ),
                    );
                }
                out.insert(server.id.clone(), Value::Object(entry));
            }
        }
    }
    if out.is_empty() {
        return None;
    }
    Some(Value::Object(
        [("mcpServers".to_string(), Value::Object(out))]
            .into_iter()
            .collect(),
    ))
}

/// Write (or clean up a stale) `.mcp.json` in a managed project cwd.
/// Returns the path written, or `None` when nothing was written.
pub fn sync_claude_mcp_json(
    cwd: &Path,
    payload: Option<&Value>,
) -> std::io::Result<Option<PathBuf>> {
    let file = cwd.join(".mcp.json");
    match payload {
        Some(payload) => {
            let text = serde_json::to_string_pretty(payload)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            std::fs::write(&file, text)?;
            Ok(Some(file))
        }
        None => {
            if file.is_symlink() || file.exists() {
                let _ = std::fs::remove_file(&file);
            }
            Ok(None)
        }
    }
}

/// Parity: `buildOpenCodeMcpConfigContent` (mcp-config.ts:455) — the
/// inline JSON for `OPENCODE_CONFIG_CONTENT` / `MIMOCODE_CONFIG_CONTENT`.
/// `None` means "leave the env untouched".
pub fn build_env_config_content(
    servers: &[McpServerConfig],
    tokens: &HashMap<String, String>,
    allowed_directories: &[String],
) -> Option<String> {
    let mut mcp = Map::new();
    for server in servers.iter().filter(|s| s.enabled) {
        match server.transport {
            McpTransport::Stdio => {
                let command = server.command.as_deref().map(str::trim).unwrap_or_default();
                if command.is_empty() {
                    continue;
                }
                let mut entry = Map::new();
                entry.insert("type".to_string(), Value::String("local".to_string()));
                let mut command_arr = vec![Value::String(command.to_string())];
                if let Some(args) = server.args.as_ref() {
                    command_arr.extend(args.iter().map(|a| Value::String(a.clone())));
                }
                entry.insert("command".to_string(), Value::Array(command_arr));
                let env = string_map(server.env.as_ref());
                if !env.is_empty() {
                    entry.insert(
                        "environment".to_string(),
                        Value::Object(
                            env.into_iter()
                                .map(|(k, v)| (k, Value::String(v)))
                                .collect(),
                        ),
                    );
                }
                entry.insert("enabled".to_string(), Value::Bool(true));
                mcp.insert(server.id.clone(), Value::Object(entry));
            }
            McpTransport::Sse | McpTransport::Http => {
                let url = server.url.as_deref().map(str::trim).unwrap_or_default();
                if url.is_empty() {
                    continue;
                }
                let mut entry = Map::new();
                entry.insert("type".to_string(), Value::String("remote".to_string()));
                entry.insert("url".to_string(), Value::String(url.to_string()));
                let headers = string_map(server.headers.as_ref());
                if let Some(merged) =
                    merge_auth_header(&headers, oauth_bearer(server, tokens).as_deref())
                {
                    entry.insert(
                        "headers".to_string(),
                        Value::Object(
                            merged
                                .into_iter()
                                .map(|(k, v)| (k, Value::String(v)))
                                .collect(),
                        ),
                    );
                }
                entry.insert("enabled".to_string(), Value::Bool(true));
                mcp.insert(server.id.clone(), Value::Object(entry));
            }
        }
    }
    let mut config = Map::new();
    if !mcp.is_empty() {
        config.insert("mcp".to_string(), Value::Object(mcp));
    }
    let mut allowlist: Vec<String> = allowed_directories
        .iter()
        .map(|dir| dir.trim().to_string())
        .filter(|dir| !dir.is_empty() && Path::new(dir).is_absolute())
        .collect();
    allowlist.sort();
    allowlist.dedup();
    if !allowlist.is_empty() {
        config.insert(
            "permission".to_string(),
            Value::Object(
                [(
                    "external_directory".to_string(),
                    Value::Object(
                        allowlist
                            .into_iter()
                            .map(|dir| (dir, Value::String("allow".to_string())))
                            .collect(),
                    ),
                )]
                .into_iter()
                .collect(),
            ),
        );
    }
    if config.is_empty() {
        return None;
    }
    Some(Value::Object(config).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::sanitize_mcp_server;

    fn servers() -> Vec<McpServerConfig> {
        // Through the real sanitizer, so fixtures carry exactly what the
        // daemon would persist (dropped invalid entries included).
        [
            serde_json::json!({
                "id": "local",
                "transport": "stdio",
                "enabled": true,
                "command": "npx",
                "args": ["-y", "x"],
                "env": { "TOKEN": "abc" }
            }),
            serde_json::json!({
                "id": "remote",
                "transport": "http",
                "enabled": true,
                "url": "https://mcp.example/",
                "authMode": "oauth",
                "headers": { "Authorization": "   ", "X-Extra": "1" }
            }),
            serde_json::json!({
                "id": "off", "transport": "stdio", "enabled": false, "command": "nope"
            }),
            serde_json::json!({ "id": "BAD ID", "transport": "stdio" }),
        ]
        .into_iter()
        .filter_map(|raw| sanitize_mcp_server(&raw))
        .collect()
    }

    #[test]
    fn claude_json_skips_disabled_and_merges_bearer() {
        let mut tokens = HashMap::new();
        tokens.insert("remote".to_string(), "tok123".to_string());
        let payload = build_claude_mcp_json(&servers(), &tokens).expect("payload");
        let servers = &payload["mcpServers"];
        assert!(servers.get("off").is_none());
        assert!(servers.get("BAD ID").is_none());
        assert_eq!(servers["local"]["command"], serde_json::json!("npx"));
        // Blank user Authorization never suppresses the daemon Bearer.
        assert_eq!(
            servers["remote"]["headers"]["Authorization"],
            serde_json::json!("Bearer tok123")
        );
        assert_eq!(servers["remote"]["headers"]["X-Extra"], serde_json::json!("1"));
    }

    #[test]
    fn empty_config_builds_nothing() {
        assert!(build_claude_mcp_json(&[], &HashMap::new()).is_none());
        assert!(build_env_config_content(&[], &HashMap::new(), &[]).is_none());
    }

    #[test]
    fn managed_cwd_guard_rejects_roots_and_foreign_dirs() {
        let root = Path::new("/data/projects");
        assert!(is_managed_project_cwd(Some("/data/projects/p1"), root));
        assert!(!is_managed_project_cwd(Some("/data/projects"), root));
        assert!(!is_managed_project_cwd(Some("/elsewhere/p1"), root));
        assert!(!is_managed_project_cwd(None, root));
    }

    #[test]
    fn sync_writes_and_cleans_up() {
        let dir = std::env::temp_dir().join(format!("od-mcp-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let payload = build_claude_mcp_json(&servers(), &HashMap::new()).expect("payload");
        let written = sync_claude_mcp_json(&dir, Some(&payload)).expect("write");
        assert!(written.is_some());
        assert!(dir.join(".mcp.json").exists());
        sync_claude_mcp_json(&dir, None).expect("clean");
        assert!(!dir.join(".mcp.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
