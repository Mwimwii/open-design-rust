//! The packaging contract: where the web export lives inside a bundle.
//!
//! `tauri.conf.json` maps `bundle.resources` `../web-export` → `web`, so the
//! bundler copies the staged export into every bundle as `web`, next to the
//! binary: a Linux deb installs it at `usr/lib/OpenDesign/web`, a macOS app
//! bundle at `Contents/Resources/web`, and a Windows build next to the exe.
//! That is the *only* place a packaged shell may read the export from — a
//! packaged app must never fall back to the source checkout it was built
//! from.
//!
//! The rules live here so the shell and the release smoke path agree on one
//! layout instead of two.

use std::path::{Path, PathBuf};

/// The directory `bundle.resources` maps the staged export to.
pub const WEB_RESOURCE_DIR: &str = "web";

/// The staged export directory `bundle.resources` reads from, relative to the
/// Tauri config.
pub const WEB_EXPORT_SOURCE: &str = "../web-export";

/// The file that makes an export a usable SPA shell.
pub const WEB_SHELL_FILE: &str = "index.html";

/// The directory inside a bundle that holds the web export.
pub fn packaged_web_dir(resource_dir: &Path) -> PathBuf {
    resource_dir.join(WEB_RESOURCE_DIR)
}

/// Whether the directory holds the SPA shell the daemon serves.
///
/// An export directory that exists but holds no shell means an API-only
/// daemon, not a window full of 404s.
pub fn has_web_shell(dir: &Path) -> bool {
    dir.join(WEB_SHELL_FILE).is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_export_lives_under_web_inside_the_resource_dir() {
        assert_eq!(
            packaged_web_dir(Path::new("/usr/lib/OpenDesign")),
            PathBuf::from("/usr/lib/OpenDesign/web")
        );
    }

    #[test]
    fn a_shell_is_an_index_html() {
        let dir = std::env::temp_dir().join("od-packaging-shell-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        assert!(!has_web_shell(&dir));
        std::fs::write(dir.join(WEB_SHELL_FILE), "<!doctype html>").expect("shell");
        assert!(has_web_shell(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
