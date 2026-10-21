//! The injected `window.__od__` script and the client facts it carries.
//!
//! Injected as a Tauri *initialization script*, which runs before any of the
//! page's own scripts: the app's first render already sees `__od__`, so
//! `isOpenDesignHostAvailable()` is true from the start and nothing has to
//! wait for a bridge to appear.
//!
//! The script is a pure pipe over the commands in this module tree. It
//! deliberately holds no policy: paths, project ids, daemon URLs and OS
//! interaction all stay on the Rust side. The two things it does own are
//! unavoidable in the page: resolving Tauri's invoke function, and turning a
//! rejected invoke (a missing command, a lost window) into the failure
//! envelope the contract declares.

use serde::Serialize;

/// Client facts the bridge publishes as `client` (`OpenDesignHostClient`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClientFacts {
    #[serde(rename = "type")]
    pub client_type: &'static str,
    /// Node-style platform name, matching what `process.platform` reported in
    /// the Electron host: `darwin`, `win32`, `linux`.
    pub platform: String,
    /// BCP-47 locale read from the environment, omitted when unknown.
    #[serde(rename = "osLocale", skip_serializing_if = "Option::is_none")]
    pub os_locale: Option<String>,
}

impl ClientFacts {
    /// Facts for the current process.
    pub fn detect() -> Self {
        Self {
            client_type: "desktop",
            platform: platform_for(std::env::consts::OS).to_string(),
            os_locale: os_locale_from_env(),
        }
    }
}

/// Map `std::env::consts::OS` onto `process.platform`'s spelling.
///
/// The renderer compares against the Electron host's values
/// (`client.platform === 'darwin'` and friends), so the Node spelling is the
/// contract here, not the Rust one.
pub fn platform_for(os: &str) -> &'static str {
    match os {
        "macos" => "darwin",
        "windows" => "win32",
        "linux" => "linux",
        "ios" => "ios",
        "android" => "android",
        "freebsd" => "freebsd",
        "openbsd" => "openbsd",
        "netbsd" => "netbsd",
        _ => "unknown",
    }
}

/// Read the OS locale the way the Electron host read `app.getLocale()`.
///
/// Electron asks the OS; the closest portable equivalent is the POSIX locale
/// environment, in the precedence the C library uses. `C` and `POSIX` mean
/// "no locale was chosen", so they are reported as unknown (the field is
/// optional and the renderer falls back to `navigator.language`).
pub fn os_locale_from_env() -> Option<String> {
    let raw = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|key| std::env::var(key).ok())
        .or_else(|| std::env::var("LANGUAGE").ok())?;
    normalize_locale(&raw)
}

/// `zh_CN.UTF-8@euro` → `zh-CN`; `C`, `POSIX` and empty → `None`.
pub fn normalize_locale(raw: &str) -> Option<String> {
    let without_modifier = raw.split('@').next().unwrap_or(raw);
    let without_charset = without_modifier
        .split('.')
        .next()
        .unwrap_or(without_modifier);
    let value = without_charset.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("c") || value.eq_ignore_ascii_case("posix") {
        return None;
    }
    Some(value.replace('_', "-"))
}

/// The complete bridge source, ready for
/// `WebviewWindowBuilder::initialization_script`.
pub fn script(facts: &ClientFacts) -> String {
    let client = serde_json::to_string(facts).expect("client facts are serializable");
    BRIDGE_SOURCE.replace("__OD_CLIENT_JSON__", &client)
}

/// The bridge, with the client facts substituted in.
///
/// Kept in one raw string so the injected script is reviewable as the single
/// artifact it is. It is written for a WebKit/WebView2/WebKitGTK page: no
/// imports, no bundler, and no `__TAURI__` global (which would require
/// enabling `withGlobalTauri` and widen the page's API surface).
const BRIDGE_SOURCE: &str = r#"(function () {
  'use strict';
  var GLOBAL = '__od__';
  var VERSION = 2;

  function reasonFromError(error) {
    if (error && typeof error === 'object' && typeof error.message === 'string' && error.message) {
      return error.message;
    }
    return String(error);
  }

  function failure(reason, details) {
    var envelope = { ok: false, reason: String(reason) };
    if (details !== undefined) envelope.details = details;
    return envelope;
  }

  // Tauri's own runtime invokes commands through this object. Resolved lazily:
  // an initialization script is not guaranteed to run after Tauri's own, and
  // every bridge call happens long after both.
  function invoke(command, args) {
    var internals = window.__TAURI_INTERNALS__;
    if (!internals || typeof internals.invoke !== 'function') {
      return Promise.reject(new Error('OpenDesign host IPC is unavailable in this window'));
    }
    return Promise.resolve(internals.invoke(command, args || {}));
  }

  // Action-style command: the answer IS the wire envelope, so anything else is
  // a malformed host response rather than a success.
  function envelope(command, args) {
    return invoke(command, args).then(function (value) {
      if (value && typeof value === 'object' && typeof value.ok === 'boolean') return value;
      return failure('host command ' + command + ' returned an invalid response', value);
    }, function (error) {
      return failure(reasonFromError(error));
    });
  }

  // Snapshot-style command (the updater surface): the answer is a status
  // object, not an envelope.
  function snapshot(command, args) {
    return invoke(command, args).then(function (value) {
      if (value && typeof value === 'object' && !Array.isArray(value)) return value;
      throw new Error('host command ' + command + ' returned an invalid status');
    });
  }

  function updaterAction(action, options) {
    return snapshot('host_updater', { action: action, options: options || null });
  }

  // No native surface ever pushes an update status in this shell, so there is
  // nothing to unsubscribe from.
  function never() {
    return function () {};
  }

  var bridge = {
    version: VERSION,
    client: __OD_CLIENT_JSON__,
    appearance: {
      // Fire-and-forget, as in the Electron preload: the host validates.
      setTheme: function (theme) {
        invoke('host_set_theme', { theme: theme }).catch(function () {});
      }
    },
    shell: {
      openExternal: function (url) {
        return envelope('open_external', { url: String(url) });
      },
      openPath: function (projectId) {
        return envelope('host_open_path', { projectId: String(projectId) });
      }
    },
    browser: {
      clearData: function (options) {
        return envelope('host_clear_browser_data', { options: options || null });
      }
    },
    capture: {
      page: function (options) {
        return envelope('host_capture_page', { options: options || null });
      }
    },
    project: {
      pickAndImport: function (init) {
        return envelope('host_pick_and_import', { init: init || null });
      },
      pickAndReplaceWorkingDir: function (projectId) {
        return envelope('host_pick_and_replace_working_dir', { projectId: String(projectId) });
      },
      pickWorkingDir: function () {
        return envelope('host_pick_working_dir', {});
      }
    },
    pdf: {
      print: function (html, nonce, options) {
        return envelope('host_print_pdf', {
          html: String(html),
          nonce: nonce === undefined || nonce === null ? null : String(nonce),
          options: options || null
        });
      }
    },
    pet: {
      setVisible: function (visible) {
        invoke('host_set_pet_visible', { visible: Boolean(visible) }).catch(function () {});
      }
    },
    updater: {
      status: function (options) { return updaterAction('status', options); },
      check: function (options) { return updaterAction('check', options); },
      'clear-cache': function (options) { return updaterAction('clear-cache', options); },
      download: function (options) { return updaterAction('download', options); },
      install: function (options) { return updaterAction('install', options); },
      quit: function (options) {
        return envelope('host_updater', { action: 'quit', options: options || null });
      },
      setMenuLabels: function (labels) {
        return envelope('host_updater', { action: 'set-menu-labels', labels: labels });
      },
      subscribe: never,
      subscribeOpenDialog: never
    }
  };

  Object.defineProperty(window, GLOBAL, {
    configurable: true,
    enumerable: true,
    value: bridge,
    writable: true
  });

  // The Electron preload exposed a second global beside `__od__`:
  // `window.openDesignDesktop = { exportDiagnostics() }`. It is deliberately
  // not part of `__od__` (the host-bridge validator in packages/host does not
  // know it); the web app only probes it to choose the native save dialog over
  // the browser download. Same result envelope as the Electron IPC handler.
  var desktop = {
    exportDiagnostics: function () {
      return invoke('host_export_diagnostics', {});
    }
  };
  Object.defineProperty(window, 'openDesignDesktop', {
    configurable: true,
    enumerable: true,
    value: desktop,
    writable: true
  });

  // `open-design:app-config-changed`: the Electron preload re-dispatched the
  // main process's `od:app-config-changed` IPC as a DOM event, which
  // apps/web/src/App.tsx listens for to re-read config. The shell emits the
  // same event over Tauri's event bus; bridge it into the page. Subscribing is
  // best-effort and deferred: an initialization script is not ordered against
  // Tauri's own, so `__TAURI_INTERNALS__` may not exist yet.
  function onHostEvent(name, handler, attempts) {
    var internals = window.__TAURI_INTERNALS__;
    if (internals && typeof internals.invoke === 'function' &&
        typeof internals.transformCallback === 'function') {
      var id = internals.transformCallback(function () { handler(); });
      Promise.resolve(internals.invoke('plugin:event|listen', {
        event: name,
        target: { kind: 'Any' },
        handler: id
      })).catch(function () {});
      return;
    }
    if ((attempts || 0) >= 50) return;
    setTimeout(function () {
      onHostEvent(name, handler, (attempts || 0) + 1);
    }, 100);
  }
  onHostEvent('od:app-config-changed', function () {
    window.dispatchEvent(new CustomEvent('open-design:app-config-changed'));
  });
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> ClientFacts {
        ClientFacts {
            client_type: "desktop",
            platform: "linux".to_string(),
            os_locale: Some("pt-BR".to_string()),
        }
    }

    #[test]
    fn client_facts_serialize_the_desktop_type() {
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&facts()).expect("serializable"))
                .expect("valid json");
        assert_eq!(json["type"], "desktop");
        assert_eq!(json["platform"], "linux");
        assert_eq!(json["osLocale"], "pt-BR");
    }

    #[test]
    fn os_locale_is_omitted_when_unknown() {
        let mut facts = facts();
        facts.os_locale = None;
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&facts).expect("serializable"))
                .expect("valid json");
        assert!(json.get("osLocale").is_none());
        assert!(json.get("type").is_some());
    }

    #[test]
    fn platform_uses_the_node_spelling() {
        assert_eq!(platform_for("macos"), "darwin");
        assert_eq!(platform_for("windows"), "win32");
        assert_eq!(platform_for("linux"), "linux");
        assert_eq!(platform_for("plan9"), "unknown");
    }

    #[test]
    fn locale_normalization() {
        assert_eq!(normalize_locale("zh_CN.UTF-8"), Some("zh-CN".to_string()));
        assert_eq!(
            normalize_locale("pt_BR.UTF-8@euro"),
            Some("pt-BR".to_string())
        );
        assert_eq!(normalize_locale("en_US"), Some("en-US".to_string()));
        assert_eq!(normalize_locale("C"), None);
        assert_eq!(normalize_locale("C.UTF-8"), None);
        assert_eq!(normalize_locale("POSIX"), None);
        assert_eq!(normalize_locale(""), None);
        assert_eq!(normalize_locale("  "), None);
    }

    #[test]
    fn script_substitutes_the_client_and_leaves_no_placeholder() {
        let script = script(&facts());
        assert!(!script.contains("__OD_"), "placeholder left in the script");
        assert!(script.contains(
            "client: {\"type\":\"desktop\",\"platform\":\"linux\",\"osLocale\":\"pt-BR\"}"
        ));
        assert!(
            script.contains("'__od__'"),
            "the global name is the contract"
        );
        assert!(script.contains("version: VERSION"));
        assert!(script.contains("VERSION = 2"));
    }

    #[test]
    fn script_declares_every_group_the_validator_requires() {
        // `isOpenDesignHostBridge` (packages/host/src/detection.ts) checks the
        // version, the client type, and these capability methods. A group that
        // is missing from the injected script makes the whole bridge
        // invisible to the app.
        let script = script(&facts());
        for method in [
            "openExternal",
            "openPath",
            "clearData",
            "pickAndImport",
            "pickAndReplaceWorkingDir",
            "setVisible",
            "subscribeOpenDialog",
            "subscribe",
            "setMenuLabels",
            "status",
            "check",
            "download",
            "install",
            "quit",
            "print",
            "page",
            "pickWorkingDir",
            "'clear-cache'",
        ] {
            assert!(
                script.contains(method),
                "bridge source no longer mentions {method}"
            );
        }
    }

    #[test]
    fn script_exposes_the_desktop_diagnostics_extra() {
        // The web app's ExportDiagnosticsButton takes the native path only when
        // `window.openDesignDesktop.exportDiagnostics` exists; without the
        // extra it silently falls back to a browser download.
        let script = script(&facts());
        assert!(script.contains("'openDesignDesktop'"));
        assert!(script.contains("exportDiagnostics"));
        assert!(script.contains("host_export_diagnostics"));
    }

    #[test]
    fn script_bridges_the_app_config_changed_event() {
        // Parity: apps/web/src/App.tsx listens for the DOM event, which the
        // Electron preload re-dispatched from its `od:app-config-changed` IPC.
        let script = script(&facts());
        assert!(script.contains("od:app-config-changed"));
        assert!(script.contains("open-design:app-config-changed"));
        assert!(script.contains("plugin:event|listen"));
        assert!(script.contains("transformCallback"));
    }

    /// Catches a syntax error in the injected script, which would otherwise
    /// only show up as an app that silently has no host bridge.
    #[test]
    fn script_parses_as_javascript_when_node_is_available() {        let script = script(&facts());
        let dir = std::env::temp_dir().join("od-tauri-bridge-check");
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let path = dir.join("bridge.js");
        if std::fs::write(&path, &script).is_err() {
            return;
        }
        let Ok(output) = std::process::Command::new("node")
            .arg("--check")
            .arg(&path)
            .output()
        else {
            // Node is not a build dependency; the check is a guard, not a gate.
            return;
        };
        assert!(
            output.status.success(),
            "injected bridge is not valid JavaScript: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
