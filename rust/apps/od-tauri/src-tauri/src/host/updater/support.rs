//! Helpers every updater module shares (parity:
//! `apps/desktop/src/main/updater/support.ts`).
//!
//! Structured error construction and the JSON type-guard primitives the feed,
//! config, and store readers parse with. Nothing here reads files or holds
//! updater state.

use serde_json::{Map, Value};

/// A structured updater error (parity: `createError`).
///
/// The renderer reads `code` and `message` off the status snapshot. The
/// Electron helper can attach free-form `details`; nothing in this host does
/// yet, and a field nothing sets would only cost size on every rejection.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateError {
    pub code: String,
    pub message: String,
}

impl UpdateError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    /// The snapshot shape: `{code, message}`.
    pub fn to_json(&self) -> Value {
        let mut object = Map::new();
        object.insert("code".into(), Value::String(self.code.clone()));
        object.insert("message".into(), Value::String(self.message.clone()));
        Value::Object(object)
    }
}

/// A non-empty string field (parity: `stringField`).
///
/// An empty string is absent on purpose: the feed writers emit `""` for a
/// field they have nothing for, and a version or URL of `""` is not a version
/// or a URL.
pub fn string_field(record: &Map<String, Value>, key: &str) -> Option<String> {
    record
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// A finite number field (parity: `numberField`).
pub fn number_field(record: &Map<String, Value>, key: &str) -> Option<f64> {
    record
        .get(key)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
}

/// An object field (parity: `objectField`).
pub fn object_field<'a>(
    record: &'a Map<String, Value>,
    key: &str,
) -> Option<&'a Map<String, Value>> {
    record.get(key).and_then(Value::as_object)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn field_readers_refuse_empty_and_wrongly_typed_values() {
        let record = json!({
            "empty": "",
            "name": "OpenDesign",
            "size": 12,
            "fraction": 1.5,
            "text": "12",
            "nested": {"a": 1},
            "list": [1],
        });
        let record = record.as_object().expect("object");
        assert_eq!(string_field(record, "name"), Some("OpenDesign".to_string()));
        assert_eq!(string_field(record, "empty"), None);
        assert_eq!(string_field(record, "missing"), None);
        assert_eq!(string_field(record, "size"), None);
        assert_eq!(number_field(record, "size"), Some(12.0));
        assert_eq!(number_field(record, "fraction"), Some(1.5));
        assert_eq!(number_field(record, "text"), None);
        assert_eq!(
            object_field(record, "nested").and_then(|nested| number_field(nested, "a")),
            Some(1.0)
        );
        assert_eq!(object_field(record, "list"), None);
    }

    #[test]
    fn an_error_snapshot_carries_the_code_and_message() {
        let plain = UpdateError::new("metadata-missing-version", "no version").to_json();
        assert_eq!(plain["code"], json!("metadata-missing-version"));
        assert_eq!(plain["message"], json!("no version"));
        assert_eq!(
            plain.as_object().expect("object").len(),
            2,
            "the snapshot carries exactly the two fields the renderer reads"
        );
    }
}
