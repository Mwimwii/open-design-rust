fn main() {
    // The shell window is a remote origin: it loads the loopback daemon, which
    // serves the apps/web export. Tauri refuses app commands to remote origins
    // unless the app declares an ACL manifest for them, so without this every
    // `invoke` from the window fails with "not allowed" — the app would have no
    // reachable IPC at all. The generated `allow-<command>` permissions are
    // what `capabilities/default.json` grants.
    tauri_build::try_build(
        tauri_build::Attributes::new().app_manifest(
            tauri_build::AppManifest::new().commands(&[
                "app_version",
                "daemon_health",
                "daemon_info",
                "open_external",
                "pick_workspace_folder",
            ]),
        ),
    )
    .expect("failed to run tauri-build");
}
