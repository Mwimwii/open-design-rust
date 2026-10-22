//! The packaging contract the release smoke path depends on.
//!
//! The smoke path builds a bundle, runs the binary out of it, and asks the
//! daemon for the web app. Those three steps only mean something if the
//! bundle really ships the export and the shell really looks for it where the
//! bundler put it — so this test pins both halves against the real
//! `tauri.conf.json` rather than against a copy of it.

use std::path::{Path, PathBuf};

use od_tauri_lib::packaging::{
    has_web_shell, packaged_web_dir, WEB_EXPORT_SOURCE, WEB_RESOURCE_DIR,
};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn tauri_config() -> serde_json::Value {
    let raw = std::fs::read_to_string(manifest_dir().join("tauri.conf.json"))
        .expect("tauri.conf.json must be readable");
    serde_json::from_str(&raw).expect("tauri.conf.json must be JSON")
}

#[test]
fn the_bundle_ships_the_staged_export_under_web() {
    let config = tauri_config();
    let resources = config["bundle"]["resources"]
        .as_object()
        .expect("bundle.resources must be a mapping");
    assert_eq!(
        resources
            .get(WEB_EXPORT_SOURCE)
            .and_then(|value| value.as_str()),
        Some(WEB_RESOURCE_DIR),
        "the staged export must land in the bundle as `{WEB_RESOURCE_DIR}`"
    );
}

#[test]
fn the_window_never_embeds_the_export() {
    // `frontendDist` exists only because the bundler requires it; the window
    // loads the daemon's origin, so the frontend asset is a placeholder.
    let config = tauri_config();
    assert_eq!(
        config["build"]["frontendDist"].as_str(),
        Some("../web-placeholder")
    );
}

#[test]
fn the_bundle_carries_the_release_identity() {
    // The version is the release channel's source, and the product name is
    // the directory the bundler installs the resources into.
    let config = tauri_config();
    assert!(
        config["version"]
            .as_str()
            .is_some_and(|value| !value.is_empty()),
        "the bundle must carry a version"
    );
    assert_eq!(
        config["productName"].as_str(),
        Some("OpenDesign"),
        "the smoke path finds the bundle's resources by the product name"
    );
    assert!(config["identifier"].as_str().is_some_and(|v| !v.is_empty()));
}

#[test]
fn the_shell_looks_for_the_export_where_the_bundler_puts_it() {
    let resource_dir = Path::new("/usr/lib/OpenDesign");
    let web_dir = packaged_web_dir(resource_dir);
    assert_eq!(web_dir, PathBuf::from("/usr/lib/OpenDesign/web"));
    // The rule that keeps a packaged app honest: a directory without the SPA
    // shell is not an export, so the shell never serves a bundle that forgot
    // to ship one.
    assert!(!has_web_shell(&web_dir));
}
