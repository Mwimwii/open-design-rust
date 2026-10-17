//! OpenDesign desktop shell (Tauri v2).
//!
//! Replaces `apps/desktop` (Electron main, ~15.7k LOC). Responsibilities
//! ported so far:
//!
//! * in-process [`od_daemon::RunningDaemon`] lifecycle (spawn / ready / shutdown),
//! * data-root resolution per `AGENTS.md` → "Daemon data directory contract"
//!   (`OD_DATA_DIR` resolved once, handed to every child process),
//! * main window creation pointing at the daemon URL (same-origin API + SPA),
//! * `od://` deep-link scheme registration,
//! * IPC commands: daemon info/health, folder picker, guarded external open.
//!
//! The window serves the `apps/web` static export from the embedded daemon
//! (transport A) instead of bundling its own frontend; the web app reaches the
//! shell through the `__od__` host bridge, injected at document start from
//! [`host::bridge`].
//!
//! Follow-ups tracked in beads: updater parity, splash/chrome parity, frame
//! capture on platforms other than Linux, and bundled-app packaging
//! verification.

mod commands;
mod host;
mod shell;

pub use shell::ShellState;

pub fn run() {
    if std::env::var_os("RUST_LOG").is_none() {
        std::env::set_var("RUST_LOG", "info,od_daemon=info");
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_http::init())
        .plugin(tauri_plugin_deep_link::init())
        .invoke_handler(tauri::generate_handler![
            commands::app_version,
            commands::daemon_health,
            commands::daemon_info,
            commands::pick_workspace_folder,
            host::shell::open_external,
            host::shell::host_open_path,
        ])
        .setup(|app| shell::setup(app))
        .build(tauri::generate_context!())
        .expect("error while building the OpenDesign desktop shell")
        .run(|app_handle, event| {
            if let tauri::RunEvent::Exit = event {
                shell::shutdown(app_handle);
            }
        });
}
