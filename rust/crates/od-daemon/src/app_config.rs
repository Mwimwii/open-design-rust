//! App preferences file — parity port of the read/write envelope in
//! `apps/daemon/src/app-config.ts` plus the two routes in
//! `apps/daemon/src/routes/media.ts` (`GET`/`PUT /api/app-config`).
//!
//! The web app persists onboarding and settings through this file, so a daemon
//! without these routes cannot finish onboarding: the Continue handler awaits
//! `onConfigPersist(...)` before it leaves the runtime setup step.
//!
//! DOCUMENTED GAP: the TypeScript writer validates every key
//! (`applyConfigValue` per field, `INVALID_APP_CONFIG_VALUE` on a value it
//! cannot mean). This port keeps the same allowed-key filter and telemetry
//! default, and stores each supplied key as given. Field-level validation,
//! the installation-id channel file, Orbit scope checks, and observer
//! notification are deferred to the settings slice.

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

/// Keys the daemon is willing to persist (parity: `ALLOWED_KEYS`).
const ALLOWED_KEYS: &[&str] = &[
    "onboardingCompleted",
    "agentId",
    "agentModels",
    "agentCliEnv",
    "agentCliEnvIntent",
    "skillId",
    "designSystemId",
    "disabledSkills",
    "disabledDesignSystems",
    "installationId",
    "telemetry",
    "privacyDecisionAt",
    "allowSilentUpdates",
    "orbit",
    "customInstructions",
    "projectLocations",
    "defaultProjectLocationId",
    "odNextStrategyMode",
    "recentLinkedDirs",
];

fn config_file(data_dir: &Path) -> PathBuf {
    data_dir.join("app-config.json")
}

/// Keep only allowed keys (parity: `filterAllowedKeys`).
fn filter_allowed_keys(raw: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    let Some(object) = raw.as_object() else {
        return out;
    };
    for key in ALLOWED_KEYS {
        if let Some(value) = object.get(*key) {
            out.insert((*key).to_string(), value.clone());
        }
    }
    out
}

/// Fresh installs (and pre-disclosure configs) read with telemetry on, so the
/// onboarding funnel emits before the disclosure modal runs
/// (parity: `applyTelemetryDefaults`).
fn apply_telemetry_defaults(mut config: Map<String, Value>) -> Map<String, Value> {
    if !config.contains_key("telemetry") {
        config.insert("telemetry".to_string(), json!({ "metrics": true, "content": true }));
    }
    config
}

/// Read the stored preferences, or the fresh-install defaults.
///
/// A missing or corrupt file reads as empty prefs, exactly like TypeScript:
/// the user loses their settings, not the daemon.
pub fn read_app_config(data_dir: &Path) -> Map<String, Value> {
    let raw = std::fs::read_to_string(config_file(data_dir)).unwrap_or_default();
    let parsed = serde_json::from_str::<Value>(&raw).unwrap_or(Value::Null);
    apply_telemetry_defaults(filter_allowed_keys(&parsed))
}

/// Merge the supplied keys over the stored config and persist the result.
///
/// Shallow per-key replacement matches `applyConfigValue` for the object-valued
/// keys the web app sends (it always sends whole objects).
pub fn write_app_config(data_dir: &Path, partial: &Value) -> Result<Map<String, Value>, String> {
    let Some(partial) = partial.as_object() else {
        return Err("app config must be an object".to_string());
    };
    let mut config = read_app_config(data_dir);
    for key in ALLOWED_KEYS {
        if let Some(value) = partial.get(*key) {
            config.insert((*key).to_string(), value.clone());
        }
    }
    std::fs::create_dir_all(data_dir).map_err(|err| err.to_string())?;
    let path = config_file(data_dir);
    let temp = data_dir.join(format!(
        "app-config.json.{}.tmp",
        std::process::id()
    ));
    let body = Value::Object(config.clone()).to_string();
    std::fs::write(&temp, body).map_err(|err| err.to_string())?;
    std::fs::rename(&temp, &path).map_err(|err| err.to_string())?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("od-app-config-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    #[test]
    fn fresh_install_reads_with_telemetry_on_and_unknown_keys_ignored() {
        let dir = temp_dir("fresh");
        let config = read_app_config(&dir);
        assert_eq!(config["telemetry"]["metrics"], true);
        assert_eq!(config["telemetry"]["content"], true);
        assert!(config.get("agentId").is_none());

        std::fs::write(
            dir.join("app-config.json"),
            r#"{"agentId":"qwen","mode":"daemon","telemetry":{"metrics":false}}"#,
        )
        .expect("write");
        let config = read_app_config(&dir);
        assert_eq!(config["agentId"], "qwen");
        // `mode` is not an allowed key: the file is a preference store, not a
        // copy of the request body.
        assert!(config.get("mode").is_none());
        // An explicit opt-out survives.
        assert_eq!(config["telemetry"]["metrics"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn writes_merge_over_the_previous_file_and_survive_a_reread() {
        let dir = temp_dir("write");
        write_app_config(
            &dir,
            &json!({ "agentId": "qwen", "onboardingCompleted": true }),
        )
        .expect("write");
        let config = write_app_config(&dir, &json!({ "agentId": "claude" })).expect("write");
        assert_eq!(config["agentId"], "claude");
        assert_eq!(config["onboardingCompleted"], true);
        let stored = read_app_config(&dir);
        assert_eq!(stored["agentId"], "claude");
        assert_eq!(stored["onboardingCompleted"], true);
        assert_eq!(stored["telemetry"]["metrics"], true);
        assert!(write_app_config(&dir, &json!([])).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_file_reads_as_empty_prefs_instead_of_failing() {
        let dir = temp_dir("corrupt");
        std::fs::write(dir.join("app-config.json"), b"{not json").expect("write");
        let config = read_app_config(&dir);
        assert!(config.get("agentId").is_none());
        assert_eq!(config["telemetry"]["content"], true);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
