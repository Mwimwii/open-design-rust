//! Redaction pass shared by every diagnostics bundle (parity:
//! `packages/diagnostics/src/redaction.ts`).
//!
//! The patterns and their ORDER matter: the HTTP auth-scheme pass must run
//! before the bare `key=value` pass, or `Authorization: Bearer <credential>`
//! is swallowed by the bare pattern at the space after `Bearer` and the
//! credential itself survives.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};

/// A key whose string value is masked outright.
fn sensitive_key_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)token|password|secret|key|dsn|authorization|cookie").expect("regex")
    })
}

/// `?token=…`-style query parameters inside a URL (or any `?`/`&`/`#` run).
fn url_query_secret_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?i)([?&#])(token|password|secret|key|dsn|api[_-]?key|auth|access_token|refresh_token|id_token)(=)([^&\s#"']*)"#,
        )
        .expect("regex")
    })
}

/// Loose `key=value` / `key: value` pairs outside a URL — env dumps, argv,
/// json-line meta. Alternatives are ordered long-first so `access_token` wins
/// over `token`.
fn bare_secret_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?i)(^|[\s,;])(access_token|refresh_token|id_token|api[_-]?key|password|secret|token|auth(?:orization)?)(=|:\s*)([^\s,;"']+)"#,
        )
        .expect("regex")
    })
}

/// Bearer / Token / Basic auth schemes embed the credential after the scheme
/// keyword, so the bare pattern above stops at the space before `Bearer`.
/// The character class covers RFC 6750 token68 plus `:` for Basic credentials.
fn http_auth_scheme_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(Bearer|Token|Basic)\s+([A-Za-z0-9._~\-+/=:]{4,})").expect("regex")
    })
}

/// Structured fragments often follow a timestamp/prefix and cannot be parsed
/// as whole JSON, so the `"key": "value"` pair is matched in the raw text.
fn structured_fragment_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?i)("[^"\n]*(?:token|password|secret|api[_-]?key|authorization|cookie|dsn)[^"\n]*"\s*:\s*)"(?:\\.|[^"\\])*""#,
        )
        .expect("regex")
    })
}

pub const REDACTED: &str = "[REDACTED]";

#[derive(Debug, Clone, Default)]
pub struct RedactionOptions {
    /// The daemon operator's username, replaced in home-directory paths.
    pub username: Option<String>,
}

/// Mask sensitive string *values* by key name, recursing through objects and
/// arrays. Non-string values are left alone (a numeric `tokenCount` is a
/// count, not a credential).
pub fn redact_json_value(value: &Value, opts: &RedactionOptions) -> Value {
    match value {
        Value::Array(entries) => {
            Value::Array(entries.iter().map(|entry| redact_json_value(entry, opts)).collect())
        }
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, raw) in map {
                if sensitive_key_re().is_match(key) {
                    if let Value::String(text) = raw {
                        if !text.is_empty() {
                            out.insert(key.clone(), Value::String(REDACTED.to_string()));
                            continue;
                        }
                    }
                }
                out.insert(key.clone(), redact_json_value(raw, opts));
            }
            Value::Object(out)
        }
        Value::String(text) => Value::String(redact_text(text, opts)),
        other => other.clone(),
    }
}

/// Redact a free-form text blob. Order is load-bearing — see the module docs.
pub fn redact_text(text: &str, opts: &RedactionOptions) -> String {
    let out = http_auth_scheme_re().replace_all(text, |caps: &regex::Captures| {
        format!("{} {REDACTED}", &caps[1])
    });
    let out = url_query_secret_re().replace_all(&out, "${1}${2}${3}[REDACTED]");
    let out = bare_secret_re().replace_all(&out, "${1}${2}${3}[REDACTED]");
    let out = structured_fragment_re().replace_all(&out, |caps: &regex::Captures| {
        format!("{}\"[REDACTED]\"", &caps[1])
    });
    redact_username(&out, opts.username.as_deref())
}

/// Replace the operator's home-directory prefix with `<USER>`.
///
/// The TypeScript original uses lookahead (`(?=[/"\s])`); the `regex` crate has
/// none, so the boundary is captured and re-emitted instead — the same result,
/// because a lookahead never consumes the character it asserts on.
fn redact_username(text: &str, username: Option<&str>) -> String {
    let Some(username) = username else {
        return text.to_string();
    };
    if username.len() < 2 {
        return text.to_string();
    }
    let escaped = regex::escape(username);
    let mut out = text.to_string();
    for (pattern, replacement) in [
        (format!(r#"(/Users/{escaped})([/"\s])"#), "/Users/<USER>${2}"),
        (
            format!(r#"(\\Users\\{escaped})([\\"\s])"#),
            r"\Users\<USER>$2",
        ),
        (format!(r#"(/home/{escaped})([/"\s])"#), "/home/<USER>${2}"),
    ] {
        let Ok(re) = Regex::new(&pattern) else {
            continue;
        };
        out = re.replace_all(&out, replacement).into_owned();
    }
    out
}

/// Parse JSON, redact it, and pretty-print; fall back to text redaction when
/// the input is not valid JSON. Parity: `redactJsonText`.
pub fn redact_json_text(text: &str, opts: &RedactionOptions) -> String {
    match serde_json::from_str::<Value>(text) {
        Ok(parsed) => serde_json::to_string_pretty(&redact_json_value(&parsed, opts))
            .unwrap_or_else(|_| redact_text(text, opts)),
        Err(_) => redact_text(text, opts),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn no_opts() -> RedactionOptions {
        RedactionOptions::default()
    }

    #[test]
    fn masks_sensitive_string_values_by_key_name() {
        let input = json!({
            "apiKey": "sk-abc123",
            "password": "hunter2",
            "nested": { "authToken": "xyz", "harmless": "data" },
            "list": [{ "secret": "shh" }, { "plain": "ok" }],
        });
        let out = redact_json_value(&input, &no_opts());
        assert_eq!(out["apiKey"], json!("[REDACTED]"));
        assert_eq!(out["password"], json!("[REDACTED]"));
        assert_eq!(out["nested"]["authToken"], json!("[REDACTED]"));
        assert_eq!(out["nested"]["harmless"], json!("data"));
        assert_eq!(out["list"][0]["secret"], json!("[REDACTED]"));
        assert_eq!(out["list"][1]["plain"], json!("ok"));
    }

    #[test]
    fn does_not_mask_non_string_sensitive_values() {
        let out = redact_json_value(&json!({ "keyId": 42, "tokenCount": 0 }), &no_opts());
        assert_eq!(out["keyId"], json!(42));
        assert_eq!(out["tokenCount"], json!(0));
    }

    #[test]
    fn masks_url_query_secrets_but_keeps_ordinary_params() {
        let line = "GET https://api.example.com/v1/foo?token=abc123&page=2";
        let out = redact_text(line, &no_opts());
        assert!(out.contains("token=[REDACTED]"), "{out}");
        assert!(out.contains("page=2"), "{out}");
    }

    #[test]
    fn masks_auth_scheme_credentials_not_just_the_scheme() {
        let bearer = redact_text("X-Custom: Bearer sk-abc.DEF-123/xyz", &no_opts());
        assert!(bearer.contains("Bearer [REDACTED]"), "{bearer}");
        assert!(!bearer.contains("sk-abc"), "{bearer}");

        let token = redact_text("X-Custom: Token deadbeef0123", &no_opts());
        assert!(token.contains("Token [REDACTED]"), "{token}");
        assert!(!token.contains("deadbeef0123"), "{token}");

        let basic = redact_text("X-Custom: Basic dXNlcjpwYXNz", &no_opts());
        assert!(basic.contains("Basic [REDACTED]"), "{basic}");
        assert!(!basic.contains("dXNlcjpwYXNz"), "{basic}");

        let auth = redact_text("Authorization: Bearer sk-abc.DEF-123/xyz", &no_opts());
        assert!(!auth.contains("sk-abc"), "{auth}");
        assert!(auth.contains("[REDACTED]"), "{auth}");
    }

    #[test]
    fn masks_auth_scheme_credentials_case_insensitively() {
        let lower = redact_text("authorization: bearer sk-abc.DEF-123", &no_opts());
        assert!(!lower.contains("sk-abc"), "{lower}");
        assert!(lower.contains("[REDACTED]"), "{lower}");

        let mixed = redact_text("X-Custom: bEaReR deadbeef0123", &no_opts());
        assert!(!mixed.contains("deadbeef0123"), "{mixed}");
        assert!(mixed.contains("[REDACTED]"), "{mixed}");

        // RFC 6750 token68 includes `~`; the whole token must go.
        let tilde = redact_text("Authorization: Bearer abcd~efgh", &no_opts());
        assert!(!tilde.contains("efgh"), "{tilde}");
        assert!(!tilde.contains('~'), "{tilde}");

        let lower_basic = redact_text("X-Custom: basic dXNlcjpwYXNz", &no_opts());
        assert!(!lower_basic.contains("dXNlcjpwYXNz"), "{lower_basic}");
        assert!(lower_basic.contains("[REDACTED]"), "{lower_basic}");
    }

    #[test]
    fn masks_access_and_refresh_tokens_in_bare_and_url_form() {
        let url = redact_text(
            "POST https://oauth/token?access_token=abc&refresh_token=xyz",
            &no_opts(),
        );
        assert!(url.contains("access_token=[REDACTED]"), "{url}");
        assert!(url.contains("refresh_token=[REDACTED]"), "{url}");

        let bare = redact_text("env: access_token=abc.def refresh_token=ghi", &no_opts());
        assert!(bare.contains("access_token=[REDACTED]"), "{bare}");
        assert!(bare.contains("refresh_token=[REDACTED]"), "{bare}");
        assert!(!bare.contains("abc.def"), "{bare}");
    }

    #[test]
    fn replaces_user_home_path_with_placeholder() {
        let opts = RedactionOptions { username: Some("alice".to_string()) };
        let out = redact_text("open file /Users/alice/Documents/work.txt failed", &opts);
        assert_eq!(out, "open file /Users/<USER>/Documents/work.txt failed");
    }

    #[test]
    fn leaves_other_users_paths_untouched() {
        let opts = RedactionOptions { username: Some("alice".to_string()) };
        let out = redact_text("saw /Users/bob/code while user is alice", &opts);
        assert!(out.contains("/Users/bob/code"), "{out}");
    }

    #[test]
    fn handles_short_or_empty_username_safely() {
        let empty = RedactionOptions { username: Some(String::new()) };
        assert!(redact_text("/Users/a/x", &empty).contains("/Users/a/x"));
        let short = RedactionOptions { username: Some("a".to_string()) };
        assert!(redact_text("/Users/a/x", &short).contains("/Users/a/x"));
    }

    #[test]
    fn redacts_windows_and_linux_home_paths() {
        let opts = RedactionOptions { username: Some("alice".to_string()) };
        let out = redact_text(r"C:\Users\alice\AppData\Local\tmp.log ok", &opts);
        assert!(out.contains(r"\Users\<USER>\AppData"), "{out}");
        assert!(!out.contains("alice"), "{out}");

        let linux = redact_text("/home/alice/work/x", &opts);
        assert_eq!(linux, "/home/<USER>/work/x");
    }

    #[test]
    fn redacts_structured_fragments_that_are_not_whole_json() {
        let line = r#"2026-01-01T00:00:00Z {"token": "abc123", "ok": true}"#;
        let out = redact_text(line, &no_opts());
        assert!(out.contains(r#""token": "[REDACTED]""#), "{out}");
        assert!(!out.contains("abc123"), "{out}");
    }

    #[test]
    fn redact_json_text_pretty_prints_redacted_json() {
        let out = redact_json_text(r#"{"token":"x","body":"ok"}"#, &no_opts());
        assert!(out.contains("[REDACTED]"), "{out}");
        assert!(out.contains('\n'), "{out}");
    }

    #[test]
    fn redact_json_text_falls_back_for_invalid_json() {
        let out = redact_json_text("not json but token=xyz here", &no_opts());
        assert!(out.contains("token=[REDACTED]"), "{out}");
    }
}
