//! Telemetry environment resolution (parity:
//! `apps/daemon/src/telemetry-environment.ts`).
//!
//! Precedence is explicit and ordered: an operator-supplied telemetry
//! environment wins over the deployment's generic one, and `NODE_ENV` is only
//! consulted as a last resort. Blank values never count as a choice, so
//! `OD_TELEMETRY_ENV=""` does not shadow a real `OPEN_DESIGN_ENV`.

pub const DEFAULT_TELEMETRY_ENV: &str = "development";

fn first_non_empty<'a>(values: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    values
        .into_iter()
        .map(str::trim)
        .find(|value| !value.is_empty())
}

/// Pure resolver; `lookup` returns the raw value for a variable.
pub fn read_telemetry_environment_with(
    lookup: impl Fn(&str) -> Option<String>,
) -> String {
    let explicit = ["OD_TELEMETRY_ENV", "OPEN_DESIGN_ENV", "POSTHOG_ENV", "LANGFUSE_ENVIRONMENT"]
        .iter()
        .filter_map(|name| lookup(name))
        .collect::<Vec<_>>();
    if let Some(value) = first_non_empty(explicit.iter().map(String::as_str)) {
        return value.to_string();
    }
    if lookup("NODE_ENV").as_deref().map(str::trim) == Some("production") {
        return "production".to_string();
    }
    DEFAULT_TELEMETRY_ENV.to_string()
}

pub fn read_telemetry_environment() -> String {
    read_telemetry_environment_with(|name| std::env::var(name).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn explicit_variables_win_in_ts_order() {
        assert_eq!(
            read_telemetry_environment_with(env(&[
                ("OD_TELEMETRY_ENV", "od"),
                ("OPEN_DESIGN_ENV", "od-env"),
                ("POSTHOG_ENV", "ph"),
                ("LANGFUSE_ENVIRONMENT", "lf"),
                ("NODE_ENV", "production"),
            ])),
            "od"
        );
        assert_eq!(
            read_telemetry_environment_with(env(&[
                ("OPEN_DESIGN_ENV", "od-env"),
                ("POSTHOG_ENV", "ph"),
            ])),
            "od-env"
        );
        assert_eq!(
            read_telemetry_environment_with(env(&[
                ("POSTHOG_ENV", "ph"),
                ("LANGFUSE_ENVIRONMENT", "lf"),
            ])),
            "ph"
        );
        assert_eq!(
            read_telemetry_environment_with(env(&[("LANGFUSE_ENVIRONMENT", "lf")])),
            "lf"
        );
    }

    #[test]
    fn blank_values_do_not_shadow_later_variables() {
        assert_eq!(
            read_telemetry_environment_with(env(&[
                ("OD_TELEMETRY_ENV", "   "),
                ("OPEN_DESIGN_ENV", ""),
                ("POSTHOG_ENV", " ph "),
            ])),
            "ph"
        );
    }

    #[test]
    fn node_env_only_decides_when_explicit_values_are_absent() {
        assert_eq!(
            read_telemetry_environment_with(env(&[("NODE_ENV", "production")])),
            "production"
        );
        assert_eq!(
            read_telemetry_environment_with(env(&[("NODE_ENV", "staging")])),
            "development"
        );
        assert_eq!(read_telemetry_environment_with(env(&[])), "development");
        assert_eq!(
            read_telemetry_environment_with(env(&[
                ("NODE_ENV", "production"),
                ("LANGFUSE_ENVIRONMENT", "lf"),
            ])),
            "lf"
        );
    }
}
