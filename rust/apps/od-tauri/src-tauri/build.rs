fn main() {
    // `bundle.resources` maps `../web-export` to `<resource_dir>/web`, and
    // `tauri-build` canonicalizes that source on every `cargo build` — before
    // any bundle step runs. A fresh checkout has no `web-export/` (it is
    // git-ignored and staged by `npm run build:web`), so a clean-clone
    // `cargo build --workspace` would fail at canonicalize. Create it here,
    // before `tauri_build`, so the mapping is always resolvable; an empty
    // directory simply copies nothing and the shell falls back to the
    // checkout's `apps/web/out`.
    let web_export = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../web-export");
    std::fs::create_dir_all(&web_export).expect("failed to create web-export staging dir");

    // The shell window is a remote origin: it loads the loopback daemon, which
    // serves the apps/web export. Tauri refuses app commands to remote origins
    // unless the app declares an ACL manifest for them, so without this every
    // `invoke` from the window fails with "not allowed" — the app would have no
    // reachable IPC at all. The generated `allow-<command>` permissions are
    // what `capabilities/default.json` grants.
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "app_version",
            "daemon_health",
            "daemon_info",
            "open_external",
            "pick_workspace_folder",
            "host_open_path",
            "host_pick_working_dir",
            "host_pick_and_import",
            "host_pick_and_replace_working_dir",
            "host_capture_page",
            "host_print_pdf",
            "host_updater",
            "host_clear_browser_data",
            "host_set_theme",
            "host_set_pet_visible",
        ]),
    ))
    .expect("failed to run tauri-build");
}
