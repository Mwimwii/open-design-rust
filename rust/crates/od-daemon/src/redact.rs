//! Lexical secret / PII scrubber (parity: `apps/daemon/src/redact.ts`).
//!
//! Every match is replaced by `[REDACTED:<kind>]` so a reviewer reading a
//! telemetry trace can see which category fired without recovering the value.
//! Pattern order matters: specific token shapes run before general ones (a
//! Langfuse `sk-lf-…` key must not be labelled as a generic `sk_key`).
//!
//! Rust's `regex` crate has no lookbehind/lookahead/backreferences, so the
//! three TS patterns that relied on them are expressed with capture groups:
//! the bearer scheme keeps its literal prefix, the phone rule consumes one
//! non-digit on each side, and the API-key header rule matches the optional
//! quote on both sides without asserting they are the same character.

use std::sync::OnceLock;

use regex::{Captures, Regex};

const PATTERNS: &[(&str, &str)] = &[
    // Langfuse public/secret keys. Must run before the generic sk- rule.
    (r"\b(?:pk|sk)-lf-[A-Za-z0-9-]{16,}\b", "langfuse_key"),
    // Anthropic / OpenAI-style keys and their OpenAI-compatible tail.
    (
        r"\bsk-(?:proj-|live-|test-|ant-)?[A-Za-z0-9_-]{20,}\b",
        "sk_key",
    ),
    // GitHub personal / OAuth / server / user-server / refresh tokens.
    (r"\bgh[opsur]_[A-Za-z0-9]{36,251}\b", "github_token"),
    // AWS access key id.
    (r"\bAKIA[0-9A-Z]{16}\b", "aws_access_key"),
    // Google API keys: AQ. before the generic sk_ rule would mis-label it.
    (r"\bAQ\.[A-Za-z0-9_-]{20,}\b", "google_api_key"),
    (r"\bAIza[0-9A-Za-z_-]{35}\b", "google_api_key"),
    // NVIDIA NGC / inference keys.
    (r"\bnvapi-[A-Za-z0-9_-]{20,}\b", "nvidia_api_key"),
    // Slack tokens.
    (r"\bxox[abprs]-[0-9A-Za-z-]{10,}\b", "slack_token"),
    // Stripe keys.
    (
        r"\b(?:sk|pk|rk)_(?:live|test)_[0-9a-zA-Z]{16,}\b",
        "stripe_key",
    ),
    // JSON Web Tokens.
    (
        r"\beyJ[A-Za-z0-9_-]+\.eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b",
        "jwt",
    ),
    // Email addresses with a TLD-like trailer.
    (r"\b[A-Za-z0-9_.+-]+@[A-Za-z0-9-]+\.[A-Za-z0-9.-]+\b", "email"),
    // IPv4, each octet gated to 0-255.
    (
        r"\b(?:(?:25[0-5]|2[0-4]\d|[01]?\d?\d)\.){3}(?:25[0-5]|2[0-4]\d|[01]?\d?\d)\b",
        "ipv4",
    ),
];

fn bearer_token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\bBearer\s+[A-Za-z0-9._~+/-]{16,}={0,2}").expect("regex")
    })
}

fn phone_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // TS: `(?<!\d)(?:…)(?!\d)`. Consume one non-digit on each side and
        // hand it back, which is equivalent for a global sweep.
        Regex::new(
            r"(^|[^0-9])((?:\+?\d{1,3}[\s.-]?)?(?:\(\d{3}\)|\d{3})[\s.-]?\d{3}[\s.-]?\d{4})([^0-9]|$)",
        )
        .expect("regex")
    })
}

fn card_candidate_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b(?:\d[ -]?){12,18}\d\b").expect("regex"))
}

fn api_key_header_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?i)(^|[^?&\w-])("?)(x-api-key|api-key|x-goog-api-key)("?)(\s*[:=]\s*)("[^"]*"|[^\s,;"'#}]+)"#,
        )
        .expect("regex")
    })
}

fn api_key_query_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?i)([?&](?:key|api_key|api-key)=)[^&#\s,;"']+"#).expect("regex")
    })
}

fn is_luhn_valid(digits: &str) -> bool {
    if !(13..=19).contains(&digits.len()) {
        return false;
    }
    let mut sum = 0u32;
    let mut alt = false;
    for byte in digits.bytes().rev() {
        if !byte.is_ascii_digit() {
            return false;
        }
        let mut value = u32::from(byte - b'0');
        if alt {
            value *= 2;
            if value > 9 {
                value -= 9;
            }
        }
        sum += value;
        alt = !alt;
    }
    sum.is_multiple_of(10)
}

fn redact_api_key_header(caps: &Captures<'_>) -> String {
    let prefix = &caps[1];
    let quote = &caps[2];
    let name = &caps[3];
    let closing_quote = &caps[4];
    let separator = &caps[5];
    let value = &caps[6];
    let redacted_value = if value.starts_with('"') {
        "\"[REDACTED:api_key_header]\""
    } else {
        "[REDACTED:api_key_header]"
    };
    format!("{prefix}{quote}{name}{closing_quote}{separator}{redacted_value}")
}

fn redact_with_counts(input: &str) -> (String, Vec<(&'static str, usize)>) {
    let mut counts: Vec<(&'static str, usize)> = Vec::new();
    let record = |counts: &mut Vec<(&'static str, usize)>, name: &'static str, count: usize| {
        if count > 0 {
            counts.push((name, count));
        }
    };

    let mut out = input.to_string();
    for (pattern, name) in PATTERNS {
        let re = Regex::new(pattern).expect("static regex");
        let mut matched = 0usize;
        out = re
            .replace_all(&out, |_: &Captures<'_>| {
                matched += 1;
                format!("[REDACTED:{name}]")
            })
            .to_string();
        record(&mut counts, name, matched);
    }

    let mut bearer = 0usize;
    out = bearer_token_re()
        .replace_all(&out, |_: &Captures<'_>| {
            bearer += 1;
            "Bearer [REDACTED:bearer_token]".to_string()
        })
        .to_string();
    record(&mut counts, "bearer_token", bearer);

    let mut phone = 0usize;
    out = phone_re()
        .replace_all(&out, |caps: &Captures<'_>| {
            phone += 1;
            format!("{}[REDACTED:phone]{}", &caps[1], &caps[3])
        })
        .to_string();
    record(&mut counts, "phone", phone);

    let mut headers = 0usize;
    out = api_key_header_re()
        .replace_all(&out, |caps: &Captures<'_>| {
            headers += 1;
            redact_api_key_header(caps)
        })
        .to_string();
    record(&mut counts, "api_key_header", headers);

    let mut queries = 0usize;
    out = api_key_query_re()
        .replace_all(&out, |caps: &Captures<'_>| {
            queries += 1;
            format!("{}[REDACTED:api_key_query]", &caps[1])
        })
        .to_string();
    record(&mut counts, "api_key_query", queries);

    let mut cards = 0usize;
    out = card_candidate_re()
        .replace_all(&out, |caps: &Captures<'_>| {
            let candidate = &caps[0];
            let digits: String = candidate.chars().filter(char::is_ascii_digit).collect();
            if is_luhn_valid(&digits) {
                cards += 1;
                "[REDACTED:credit_card]".to_string()
            } else {
                candidate.to_string()
            }
        })
        .to_string();
    record(&mut counts, "credit_card", cards);

    (out, counts)
}

/// Returns `input` with every recognised secret / PII pattern replaced.
/// Idempotent: re-running only matches new tokens.
pub fn redact_secrets(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    redact_with_counts(input).0
}

/// Same as [`redact_secrets`] plus per-category counts for an audit summary.
pub fn redact_secrets_with_counts(input: &str) -> (String, Vec<(&'static str, usize)>) {
    if input.is_empty() {
        return (String::new(), Vec::new());
    }
    redact_with_counts(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_specific_token_shapes_before_generic_ones() {
        assert_eq!(
            redact_secrets("key sk-lf-1234567890abcdefgh"),
            "key [REDACTED:langfuse_key]"
        );
        assert_eq!(
            redact_secrets("key sk-proj-12345678901234567890"),
            "key [REDACTED:sk_key]"
        );
        assert_eq!(
            redact_secrets("token ghp_123456789012345678901234567890123456"),
            "token [REDACTED:github_token]"
        );
        assert_eq!(
            redact_secrets("aws AKIA1234567890ABCDEF"),
            "aws [REDACTED:aws_access_key]"
        );
    }

    #[test]
    fn redacts_bearer_tokens_without_eating_the_scheme() {
        assert_eq!(
            redact_secrets("Authorization: Bearer abcdefghijklmnop.qrstuvwx"),
            "Authorization: Bearer [REDACTED:bearer_token]"
        );
    }

    #[test]
    fn redacts_emails_and_ipv4() {
        assert_eq!(
            redact_secrets("mail alice@example.com host 10.1.2.3"),
            "mail [REDACTED:email] host [REDACTED:ipv4]"
        );
    }

    #[test]
    fn redacts_us_phone_shapes_with_ts_parity_on_digit_run_length() {
        assert_eq!(
            redact_secrets("call (415) 555-1234 now"),
            "call [REDACTED:phone] now"
        );
        assert_eq!(
            redact_secrets("+1 415-555-1234 office"),
            "[REDACTED:phone] office"
        );
        // TS parity: the optional 1-3 digit country code lets a 11-13 digit
        // run match (the last 10 digits are the number), so these ARE
        // redacted by `apps/daemon/src/redact.ts` too.
        assert_eq!(redact_secrets("id 41555512345"), "id [REDACTED:phone]");
        // 14+ digits exceed country code + 10, so the lookahead in TS and the
        // consumed trailing non-digit here both refuse the match.
        assert_eq!(
            redact_secrets("id 41555512345678"),
            "id 41555512345678"
        );
    }

    #[test]
    fn api_key_headers_and_query_parameters_are_masked() {
        assert_eq!(
            redact_secrets("x-api-key: super-secret-value"),
            "x-api-key: [REDACTED:api_key_header]"
        );
        assert_eq!(
            redact_secrets("X-Goog-Api-Key: \"quoted-secret\""),
            "X-Goog-Api-Key: \"[REDACTED:api_key_header]\""
        );
        assert_eq!(
            redact_secrets("https://x.test/v1?key=abcd1234&model=x"),
            "https://x.test/v1?key=[REDACTED:api_key_query]&model=x"
        );
    }

    #[test]
    fn credit_cards_need_a_luhn_checksum() {
        assert_eq!(
            redact_secrets("card 4111 1111 1111 1111"),
            "card [REDACTED:credit_card]"
        );
        assert_eq!(
            redact_secrets("id 1234 5678 9012 3456"),
            "id 1234 5678 9012 3456"
        );
    }

    #[test]
    fn counts_report_each_category() {
        let (redacted, counts) = redact_secrets_with_counts(
            "alice@example.com and bob@example.com use sk-abcdefghijklmnopqrstuvwx",
        );
        assert!(redacted.contains("[REDACTED:email]"));
        assert!(redacted.contains("[REDACTED:sk_key]"));
        assert!(counts.contains(&("email", 2)));
        assert!(counts.contains(&("sk_key", 1)));
    }

    #[test]
    fn redaction_is_idempotent() {
        let once = redact_secrets("alice@example.com sk-abcdefghijklmnopqrstuvwx");
        assert_eq!(redact_secrets(&once), once);
    }
}
