use std::path::PathBuf;

use od_core::{OdError, RuntimePaths};

/// Default port of the TypeScript daemon (`OD_PORT` fallback, 7456).
pub const DEFAULT_PORT: u16 = 7456;

/// Everything the daemon needs to boot, resolved once from the environment.
///
/// Env-var names and defaults mirror `apps/daemon`: `OD_PORT`, `OD_BIND_HOST`,
/// `OD_API_TOKEN`, `OD_DISABLE_API_AUTH`, `OD_WEB_DIST_DIR`.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// Resolved data root (`OD_DATA_DIR`); every data path derives from it.
    pub paths: RuntimePaths,
    /// Loopback by default; LAN exposure requires an explicit `OD_BIND_HOST`.
    pub bind_host: String,
    /// `0` picks an ephemeral port (tests, desktop embedding).
    pub port: u16,
    /// `OD_API_TOKEN`; when set, non-loopback requests must authenticate.
    pub api_token: Option<String>,
    /// `OD_DISABLE_API_AUTH` truthy → token checks off.
    pub api_auth_disabled: bool,
    /// Static SPA directory to serve (the TypeScript daemon serves the web
    /// build; the desktop shell supplies the packaged export).
    pub web_dist: Option<PathBuf>,
}

impl DaemonConfig {
    pub fn from_env() -> Result<Self, OdError> {
        let paths = RuntimePaths::from_env()?;
        Ok(Self {
            paths,
            bind_host: nonempty_env("OD_BIND_HOST").unwrap_or_else(|| "127.0.0.1".to_string()),
            port: nonempty_env("OD_PORT")
                .and_then(|value| value.parse().ok())
                .unwrap_or(DEFAULT_PORT),
            api_token: nonempty_env("OD_API_TOKEN"),
            api_auth_disabled: truthy_env("OD_DISABLE_API_AUTH"),
            web_dist: nonempty_env("OD_WEB_DIST_DIR").map(PathBuf::from),
        })
    }

    /// The API token only guards when set and auth is not explicitly disabled
    /// (same rule as the TypeScript `isApiTokenMiddlewareEnabled`).
    pub fn active_api_token(&self) -> Option<&str> {
        if self.api_auth_disabled {
            return None;
        }
        self.api_token.as_deref().filter(|token| !token.is_empty())
    }
}

fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|value| value.trim().to_string()).filter(|value| !value.is_empty())
}

fn truthy_env(name: &str) -> bool {
    matches!(
        nonempty_env(name).as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}
