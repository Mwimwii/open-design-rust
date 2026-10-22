//! Shell lifecycle: data-root resolution, embedded daemon, main window.
//!
//! The window is not a Tauri frontend asset: it points at the embedded
//! loopback daemon, which serves the `apps/web` static export (the same origin
//! for the SPA and `/api`), the way `apps/desktop` points at the TypeScript
//! daemon. `tauri.conf.json`'s `frontendDist` exists only because the bundler
//! requires it — the app-origin assets it embeds are never loaded.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use od_core::release::{ReleaseChannel, ReleasePlatform};
use od_core::{NamespacePaths, RuntimePaths};
use od_daemon::{DaemonConfig, RunningDaemon, StartError};
use tauri::{App, AppHandle, Listener, Manager, WebviewUrl, WebviewWindowBuilder};
use url::Url;

use crate::host::desktop_auth::DesktopAuth;

/// The release identity of this build: the channel it belongs to, the
/// namespace it writes into, and the paths that namespace owns.
///
/// Parity: the packaged layout of `apps/packaged/src/paths.ts` and the
/// release domain of `packages/release/src/index.ts`. The namespace is what
/// keeps two builds apart: a stable build and a beta build, or a mac build
/// and a Windows build, own separate data roots.
#[derive(Debug, Clone)]
pub struct ShellIdentity {
    /// The version this build carries (the bundle's version).
    pub app_version: String,
    /// The release channel the version names; the stable lane when the
    /// version names none.
    pub channel: ReleaseChannel,
    /// The namespace this build writes into.
    pub namespace: String,
    /// Every path the namespace owns.
    pub paths: NamespacePaths,
}

/// State handed to IPC commands.
pub struct ShellState {
    /// Base URL of the embedded loopback daemon (e.g. `http://127.0.0.1:7456`).
    pub daemon_url: String,
    /// Resolved `OD_DATA_DIR` for this app namespace.
    pub data_dir: PathBuf,
    /// The release identity of this build.
    pub identity: ShellIdentity,
    /// The static web export the daemon serves; `None` means API-only.
    pub web_dist: Option<PathBuf>,
    /// The per-process secret the project flows mint import tokens with. In
    /// Electron this is also registered with the daemon over sidecar IPC; the
    /// Rust daemon has neither import routes nor an auth gate yet, so there is
    /// nothing to register it with.
    pub desktop_auth: DesktopAuth,
    /// Kept for graceful shutdown on `RunEvent::Exit`.
    daemon: Mutex<Option<RunningDaemon>>,
}

/// Tauri `setup` hook: resolve the data root, boot the daemon, open the
/// main window pointed at the daemon (same-origin SPA + API).
pub fn setup(app: &mut App) -> Result<(), Box<dyn std::error::Error>> {
    let identity = resolve_identity(app)?;
    let data_dir = identity.paths.data_root.clone();

    // Daemon data contract: resolve the root once, hand the same value to
    // every agent subprocess via the environment.
    std::env::set_var("OD_DATA_DIR", &data_dir);

    let paths = RuntimePaths::resolve(&data_dir).map_err(|err| err.to_string())?;
    let mut config = DaemonConfig::with_paths(paths);
    let web_dist = web_dist(app);
    config.web_dist = web_dist.clone();
    // The desktop import-token secret: one per process, minted before the
    // daemon starts so the embedded daemon can verify the tokens this shell
    // hands the renderer (parity: `REGISTER_DESKTOP_AUTH` in the Electron
    // pair, where the secret crosses the sidecar boundary instead).
    let desktop_auth = DesktopAuth::generate();
    config.desktop_auth_secret = Some(desktop_auth.secret().to_vec());
    let daemon = start_daemon(config)?;
    let daemon_url = daemon.url().to_string();

    tracing::info!(
        version = %identity.app_version,
        channel = %identity.channel,
        namespace = %identity.namespace,
        data_dir = %identity.paths.data_root.display(),
        "desktop shell identity"
    );

    app.manage(ShellState {
        daemon_url: daemon_url.clone(),
        data_dir,
        identity,
        web_dist,
        desktop_auth,
        daemon: Mutex::new(Some(daemon)),
    });

    let window_url = dev_url()
        .unwrap_or_else(|| Url::parse(&daemon_url).expect("embedded daemon URL must parse"));

    // The window's page is the apps/web export, so the shell injects the
    // `__od__` host bridge before the page's own scripts run: the app's first
    // render already sees a host.
    let bridge_script = crate::host::bridge::script(&crate::host::bridge::ClientFacts::detect());

    WebviewWindowBuilder::new(app.handle(), "main", WebviewUrl::External(window_url))
        .title("OpenDesign")
        .inner_size(1440.0, 900.0)
        .min_inner_size(960.0, 640.0)
        .initialization_script(bridge_script)
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

/// The release identity of this build: the channel it belongs to, the
/// namespace it writes into, and the paths that namespace owns.
///
/// Parity: `defaultNamespaceForAppVersion` in `tools/pack/src/config/index.ts`
/// and the packaged layout of `apps/packaged/src/paths.ts`. A build whose
/// version names a release channel owns that channel's namespace; every other
/// build (a development build, a local build) owns the default namespace, so
/// a nightly build never writes into a shipped release's data root.
fn resolve_identity(app: &App) -> Result<ShellIdentity, Box<dyn std::error::Error>> {
    // The packaged layout keeps the namespace roots one level under the app's
    // data directory: `<app data>/namespaces/<namespace>/data`.
    let app_data_dir = app.path().app_data_dir()?;
    let namespace_base_root = app_data_dir.join(od_core::namespace::NAMESPACES_DIR_NAME);
    let resource_root = app
        .path()
        .resource_dir()
        .unwrap_or_else(|_| app_data_dir.clone());
    // `OD_DATA_DIR` is the operator's override. It names a *base* root, the
    // same way the packaged runtime reads it: the namespace's data root
    // lives inside it unless it already points at this namespace's data root.
    let data_dir_override = std::env::var_os("OD_DATA_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let identity = identity_for_version(
        &app.package_info().version.to_string(),
        &namespace_base_root,
        &resource_root,
        data_dir_override.as_deref(),
    )?;
    Ok(identity)
}

/// The identity a version buys: the channel it belongs to, the namespace it
/// writes into, and the paths that namespace owns.
///
/// Split out of [`resolve_identity`] so the derivation is testable without a
/// Tauri application: the version is the only input that comes from the app.
fn identity_for_version(
    app_version: &str,
    namespace_base_root: &Path,
    resource_root: &Path,
    data_dir_override: Option<&Path>,
) -> Result<ShellIdentity, od_core::NamespaceError> {
    let named_channel = od_core::release::release_channel_from_version(Some(app_version));
    // A version that names no channel is the stable lane's version shape but
    // not a published release; the updater still reports it as the stable
    // lane, while the namespace stays the default one.
    let channel = named_channel.clone().unwrap_or(ReleaseChannel::Stable);
    let namespace = match &named_channel {
        Some(channel) => od_core::release::release_namespace(channel, ReleasePlatform::current()),
        None => od_core::namespace::DEFAULT_NAMESPACE.to_string(),
    };
    let paths = od_core::namespace::resolve_namespace_paths(
        namespace_base_root,
        &namespace,
        resource_root,
        data_dir_override,
    )?;

    Ok(ShellIdentity {
        app_version: app_version.to_string(),
        channel,
        namespace,
        paths,
    })
}

/// Bind the daemon, falling back to an ephemeral port if the preferred port
/// (default 7456, same as the TypeScript daemon) is taken.
///
/// The fallback stays because the shell's capability grants Tauri API access
/// to the loopback origin with a port wildcard (`http://127.0.0.1:*` in
/// `capabilities/default.json`), so a different port is still a granted
/// origin rather than a silently degraded window.
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
            tauri::async_runtime::block_on(RunningDaemon::start(config))
                .map_err(|err| err.to_string())
        }
        Err(err) => Err(err.to_string()),
    }
}

/// Dev-server override for HMR development
/// (`OD_UI_DEV_URL=http://localhost:3000`, e.g. `pnpm tools-dev run web`).
fn dev_url() -> Option<Url> {
    std::env::var("OD_UI_DEV_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .and_then(|value| Url::parse(&value).ok())
}

/// Static web export the daemon serves at `/` (parity: `STATIC_DIR` in
/// `apps/daemon/src/server.ts`). Resolution order:
///
/// 1. `OD_WEB_DIST_DIR` — explicit override (dev, ops, tests).
/// 2. `<resource_dir>/web` — the packaged location. `bundle.resources` maps
///    `../web-export` here and `build.rs` keeps that source dir present so a
///    clean-clone `cargo build` works; `npm run build:web` stages the export
///    into `web-export/`, and `tauri-build` copies it to `<resource_dir>/web`
///    on every build, so this is what a bundled app and `tauri dev` both take.
/// 3. `../dist` next to this crate — legacy manual staging location.
/// 4. the source checkout's `apps/web/out` — a repo build of the web app, so
///    `cargo run` needs no copy step.
///
/// Every step must contain an `index.html`: a directory that exists but holds
/// no shell means `None` (API-only daemon) rather than a window full of 404s.
pub fn web_dist(app: &App) -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("OD_WEB_DIST_DIR").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(value));
    }
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));

    let packaged = app
        .path()
        .resource_dir()
        .ok()
        .map(|dir| crate::packaging::packaged_web_dir(&dir))
        .filter(|dir| crate::packaging::has_web_shell(dir));
    if packaged.is_some() {
        return packaged;
    }

    let app_dist = manifest_dir.join("../dist");
    if crate::packaging::has_web_shell(&app_dist) {
        return Some(app_dist);
    }

    // `apps/web/out` in the source checkout: src-tauri → od-tauri → apps →
    // rust → repository root.
    let checkout_export = manifest_dir.join("../../../../apps/web/out");
    crate::packaging::has_web_shell(&checkout_export).then_some(checkout_export)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_root() -> PathBuf {
        PathBuf::from("/home/user/.local/share/ai.open-design.desktop/namespaces")
    }

    #[test]
    fn a_development_build_owns_the_default_namespace() {
        let identity =
            identity_for_version("0.1.0", &base_root(), Path::new("/app/resources"), None)
                .expect("identity");
        assert_eq!(identity.namespace, "default");
        // The channel a plain version does not name is the stable lane's
        // shape; the updater reports it as such.
        assert_eq!(identity.channel, ReleaseChannel::Stable);
        assert_eq!(
            identity.paths.data_root,
            base_root().join("default").join("data")
        );
    }

    #[test]
    fn a_release_build_owns_its_channels_namespace() {
        let identity = identity_for_version(
            "1.2.3-prerelease.4",
            &base_root(),
            Path::new("/app/resources"),
            None,
        )
        .expect("identity");
        assert_eq!(identity.channel, ReleaseChannel::Prerelease);
        assert_eq!(
            identity.namespace,
            od_core::release::release_namespace(
                &ReleaseChannel::Prerelease,
                ReleasePlatform::current()
            )
        );
        assert_eq!(
            identity.paths.data_root,
            base_root().join(&identity.namespace).join("data")
        );
    }

    #[test]
    fn the_data_dir_override_names_a_base_root() {
        let identity = identity_for_version(
            "0.1.0",
            &base_root(),
            Path::new("/app/resources"),
            Some(Path::new("/var/lib/opendesign")),
        )
        .expect("identity");
        assert_eq!(
            identity.paths.data_root,
            PathBuf::from("/var/lib/opendesign/namespaces/default/data")
        );
    }

    #[test]
    fn a_data_dir_override_scoped_to_another_namespace_is_refused() {
        let error = identity_for_version(
            "0.1.0",
            &base_root(),
            Path::new("/app/resources"),
            Some(Path::new(
                "/var/lib/opendesign/namespaces/release-beta/data",
            )),
        )
        .expect_err("mismatch");
        assert!(matches!(
            error,
            od_core::NamespaceError::DataDirNamespaceMismatch { .. }
        ));
    }
}
