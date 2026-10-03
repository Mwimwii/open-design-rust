//! Shell lifecycle: data-root resolution, embedded daemon, main window.

use std::path::PathBuf;
use std::sync::Mutex;

use od_core::RuntimePaths;
use od_daemon::{DaemonConfig, RunningDaemon, StartError};
use tauri::{App, AppHandle, Listener, Manager, WebviewUrl, WebviewWindowBuilder};
use url::Url;

/// State handed to IPC commands.
pub struct ShellState {
    /// Base URL of the embedded loopback daemon (e.g. `http://127.0.0.1:7456`).
    pub daemon_url: String,
    /// Resolved `OD_DATA_DIR` for this app namespace.
    pub data_dir: PathBuf,
    /// Kept for graceful shutdown on `RunEvent::Exit`.
    daemon: Mutex<Option<RunningDaemon>>,
}

/// Tauri `setup` hook: resolve the data root, boot the daemon, open the
/// main window pointed at the daemon (same-origin SPA + API).
pub fn setup(app: &mut App) -> Result<(), Box<dyn std::error::Error>> {
    let data_dir = resolve_data_dir(app)?;

    // Daemon data contract: resolve the root once, hand the same value to
    // every agent subprocess via the environment.
    std::env::set_var("OD_DATA_DIR", &data_dir);

    let paths = RuntimePaths::resolve(&data_dir).map_err(|err| err.to_string())?;
    let daemon = start_daemon(DaemonConfig::with_paths(paths))?;
    let daemon_url = daemon.url().to_string();

    app.manage(ShellState {
        daemon_url: daemon_url.clone(),
        data_dir,
        daemon: Mutex::new(Some(daemon)),
    });

    let window_url = dev_url().unwrap_or_else(|| {
        Url::parse(&daemon_url).expect("embedded daemon URL must parse")
    });

    WebviewWindowBuilder::new(app.handle(), "main", WebviewUrl::External(window_url))
        .title("OpenDesign")
        .inner_size(1440.0, 900.0)
        .min_inner_size(960.0, 640.0)
        .build()?;

    // `od://` invite/deeplink arrival → log + notify the webview.
    // Registration itself lives in tauri.conf.json (`plugins.deep-link`).
    let handle = app.handle().clone();
    app.listen("deep-link://new-url/", move |event| {
        let raw = event.payload().to_string();
        tracing::info!(payload = %raw, "deep link received");
        if let Some(window) = handle.get_webview_window("main") {
            let detail = serde_json::to_string(&raw).unwrap_or_else(|_| "\"\"".to_string());
            let _ = window.eval(format!(
                "window.dispatchEvent(new CustomEvent('od-deeplink', {{detail: {detail}}}));"
            ));
        }
    });

    Ok(())
}

/// Graceful shutdown: stop the embedded daemon when the app exits.
pub fn shutdown(app_handle: &AppHandle) {
    let Some(state) = app_handle.try_state::<ShellState>() else {
        return;
    };
    let Ok(mut guard) = state.daemon.lock() else {
        return;
    };
    if let Some(daemon) = guard.take() {
        tracing::info!("shutting down embedded od-daemon");
        tauri::async_runtime::block_on(daemon.shutdown());
    }
}

/// Data root precedence: explicit `OD_DATA_DIR` (dev/ops override) beats the
/// app-namespace directory. The daemon never computes its own path — the
/// shell resolves it here (AGENTS.md data contract).
fn resolve_data_dir(app: &App) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(value) = std::env::var_os("OD_DATA_DIR").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(value));
    }
    Ok(app.path().app_data_dir()?)
}

/// Bind the daemon, falling back to an ephemeral port if the preferred port
/// (default 7456, same as the TypeScript daemon) is taken.
fn start_daemon(mut config: DaemonConfig) -> Result<RunningDaemon, String> {
    match tauri::async_runtime::block_on(RunningDaemon::start(config.clone())) {
        Ok(daemon) => Ok(daemon),
        Err(StartError::Bind { addr, source }) if config.port != 0 => {
            tracing::warn!(
                port = addr.port(),
                error = %source,
                "preferred daemon port busy; falling back to an ephemeral port"
            );
            config.port = 0;
            tauri::async_runtime::block_on(RunningDaemon::start(config)).map_err(|err| err.to_string())
        }
        Err(err) => Err(err.to_string()),
    }
}

/// Vite dev server override for HMR development (`OD_UI_DEV_URL=http://localhost:5173`).
fn dev_url() -> Option<Url> {
    std::env::var("OD_UI_DEV_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .and_then(|value| Url::parse(&value).ok())
}

/// Static SPA directory the daemon serves. `OD_WEB_DIST_DIR` wins; otherwise
/// the vite build next to this crate (`../ui/dist`) when it exists.
/// Packaging will move this to the bundle resource dir (tracked in beads).
pub fn web_dist() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("OD_WEB_DIST_DIR").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(value));
    }
    let dev_dist = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../ui/dist");
    dev_dist.is_dir().then_some(dev_dist)
}
