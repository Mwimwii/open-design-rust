//! Minimal YAML front-matter parser — parity port of
//! `apps/daemon/src/design-systems/frontmatter.ts`.
//!
//! SKILL.md and DESIGN.md front matter only use a small YAML subset: scalars,
//! block literals (`|`), inline arrays (`[a, b]`), and flush-left or indented
//! `- item` sequences. This module reproduces that subset exactly, including
//! the TypeScript parser's quirks, because the catalog summaries are derived
//! from the same documents the TypeScript daemon reads:
//!
//! * a `key:` with no value opens a nested container even when the next lines
//!   are `- item` at the SAME indentation as the key (the flush-left sequence
//!   style), which is why the dedent loop keeps an empty pending container;
//! * `|`, `|-`, `>` and `>-` all collect the same block text (`>` is not
//!   folded, matching the TypeScript subset);
//! * `coerce` strips one pair of surrounding quotes, then resolves
//!   `true`/`false`/`null`/`~` and base-10 ints/floats, leaving everything
//!   else as the trimmed string.
//!
//! DOCUMENTED DEVIATIONS
//!
//! * Object key order is not preserved (serde_json's map); every consumer
//!   reads by key, and the TypeScript parser's order was never part of the
//!   contract.
//! * A malformed nesting the TypeScript parser rejects with a thrown error is
//!   dropped here instead of failing the whole document: discovery must not
//!   lose every sibling skill because one folder has a bad SKILL.md.

use serde_json::{Map, Value};

/// The two halves of a front-matter document.
pub struct ParsedFrontmatter {
    pub data: Map<String, Value>,
    pub body: String,
}

/// Parity: `parseFrontmatter` (`frontmatter.ts:17`).
///
/// A document without a leading `---` fence is all body and no data, exactly
/// like the TypeScript parser.
pub fn parse_frontmatter(src: &str) -> ParsedFrontmatter {
    let text = src.strip_prefix('\u{feff}').unwrap_or(src);
    let mut lines = text.split('\n');
    let Some(first) = lines.next() else {
        return ParsedFrontmatter {
            data: Map::new(),
            body: text.to_string(),
        };
    };
    if strip_cr(first) != "---" {
        return ParsedFrontmatter {
            data: Map::new(),
            body: text.to_string(),
        };
    }
    let rest: Vec<&str> = lines.collect();
    let Some(end) = rest.iter().position(|line| strip_cr(line) == "---") else {
        return ParsedFrontmatter {
            data: Map::new(),
            body: text.to_string(),
        };
    };
    let yaml = rest[..end].join("\n");
    let body = rest[end + 1..].join("\n");
    ParsedFrontmatter {
        data: parse_yaml_subset(&yaml),
        body,
    }
}

fn strip_cr(line: &str) -> &str {
    line.strip_suffix('\r').unwrap_or(line)
}

enum Container {
    Object(Map<String, Value>),
    Array(Vec<Value>),
}

struct StackEntry {
    indent: usize,
    container: Container,
    /// `Some` when the container is not attached to its parent yet; `None`
    /// for an array item that already occupies its slot as a placeholder.
    key: Option<String>,
}

/// Attach a finished container to its parent.
///
/// An array item was pushed as a `Null` placeholder when it was opened (the
/// TypeScript parser shares the object by reference), so it is replaced here.
fn attach(entry: StackEntry, parent: &mut Container) {
    let value = container_to_value(entry.container);
    match entry.key {
        Some(key) => {
            if let Container::Object(map) = parent {
                map.insert(key, value);
            }
        }
        None => {
            if let Container::Array(items) = parent {
                if let Some(last) = items.last_mut() {
                    *last = value;
                } else {
                    items.push(value);
                }
            }
        }
    }
}

fn container_to_value(container: Container) -> Value {
    match container {
        Container::Object(map) => Value::Object(map),
        Container::Array(items) => Value::Array(items),
    }
}

fn is_empty_object(container: &Container) -> bool {
    matches!(container, Container::Object(map) if map.is_empty())
}

fn is_array(container: &Container) -> bool {
    matches!(container, Container::Array(_))
}

/// Parity: `parseYamlSubset` (`frontmatter.ts:32`).
fn parse_yaml_subset(src: &str) -> Map<String, Value> {
    let lines: Vec<&str> = src.split('\n').map(strip_cr).collect();
    let mut stack: Vec<StackEntry> = vec![StackEntry {
        indent: 0,
        container: Container::Object(Map::new()),
        key: None,
    }];
    let mut i = 0usize;

    while i < lines.len() {
        let raw = lines[i];
        let trimmed_start = raw.trim_start();
        if trimmed_start.is_empty() || trimmed_start.starts_with('#') {
            i += 1;
            continue;
        }
        let indent = raw.chars().count() - trimmed_start.chars().count();
        let line = trimmed_start;

        // A flush-left `- item` belongs to the pending key at its own indent.
        let is_seq_item = line.starts_with("- ");
        while stack.len() > 1 && indent <= stack.last().map(|e| e.indent).unwrap_or(0) {
            let entry = stack.last().expect("non-empty stack");
            if is_seq_item
                && indent == entry.indent
                && entry.key.is_some()
                && (is_array(&entry.container) || is_empty_object(&entry.container))
            {
                break;
            }
            let entry = stack.pop().expect("non-empty stack");
            let parent = &mut stack.last_mut().expect("root entry").container;
            attach(entry, parent);
        }

        let top = stack.last_mut().expect("root entry");

        // Sequence item.
        if let Some(item) = line.strip_prefix("- ") {
            let value = item.trim();
            if matches!(top.container, Container::Object(_)) {
                // Convert the pending key's value to an array on the first
                // `-`; without a pending key there is nothing to attach to.
                if top.key.is_none() {
                    i += 1;
                    continue;
                }
                top.container = Container::Array(Vec::new());
            }
            let Container::Array(items) = &mut top.container else {
                i += 1;
                continue;
            };
            if let Some(colon) = value.find(':') {
                let key = value[..colon].trim().to_string();
                let val_raw = value[colon + 1..].trim();
                let mut object = Map::new();
                if !val_raw.is_empty() {
                    object.insert(key, coerce(val_raw));
                }
                items.push(Value::Null);
                stack.push(StackEntry {
                    indent,
                    container: Container::Object(object),
                    key: None,
                });
            } else {
                items.push(coerce(value));
            }
            i += 1;
            continue;
        }

        // `key: value` / `key:` / `key: |`.
        let Some(colon) = line.find(':') else {
            i += 1;
            continue;
        };
        if colon == 0 {
            i += 1;
            continue;
        }
        let key = line[..colon].trim().to_string();
        let val = line[colon + 1..].trim_start();

        if val.is_empty() {
            if matches!(top.container, Container::Array(_)) {
                i += 1;
                continue;
            }
            stack.push(StackEntry {
                indent,
                container: Container::Object(Map::new()),
                key: Some(key),
            });
            i += 1;
            continue;
        }

        if val == "|" || val == "|-" || val == ">" || val == ">-" {
            let mut collected: Vec<String> = Vec::new();
            let mut block_indent: Option<usize> = None;
            i += 1;
            while i < lines.len() {
                let next = lines[i];
                if next.trim().is_empty() {
                    collected.push(String::new());
                    i += 1;
                    continue;
                }
                let next_indent = next.chars().count() - next.trim_start().chars().count();
                if next_indent <= indent {
                    break;
                }
                let base = *block_indent.get_or_insert(next_indent);
                if next_indent < base {
                    break;
                }
                collected.push(next.chars().skip(base).collect());
                i += 1;
            }
            let text = collected.join("\n").trim_end().to_string();
            if let Container::Object(map) = &mut top.container {
                map.insert(key, Value::String(text));
            }
            continue;
        }

        if val == "[]" {
            if let Container::Object(map) = &mut top.container {
                map.insert(key, Value::Array(Vec::new()));
            }
            i += 1;
            continue;
        }

        if val.starts_with('[') && val.ends_with(']') && val.chars().count() >= 2 {
            let inner: String = val.chars().skip(1).take(val.chars().count() - 2).collect();
            let values: Vec<Value> = split_inline_array_items(&inner)
                .into_iter()
                .map(|item| coerce(item.trim()))
                .filter(|value| value != &Value::String(String::new()))
                .collect();
            if let Container::Object(map) = &mut top.container {
                map.insert(key, Value::Array(values));
            }
            i += 1;
            continue;
        }

        if let Container::Object(map) = &mut top.container {
            map.insert(key, coerce(val));
        }
        i += 1;
    }

    while stack.len() > 1 {
        let entry = stack.pop().expect("non-empty stack");
        let parent = &mut stack.last_mut().expect("root entry").container;
        attach(entry, parent);
    }
    match stack.pop().expect("root entry").container {
        Container::Object(map) => map,
        Container::Array(_) => Map::new(),
    }
}

/// Parity: `splitInlineArrayItems` (`frontmatter.ts:159`). A quote only opens
/// a quoted element when it is that element's first non-whitespace character,
/// so an apostrophe inside a plain scalar stays literal.
fn split_inline_array_items(inner: &str) -> Vec<String> {
    let mut items: Vec<String> = Vec::new();
    let mut buffer = String::new();
    let mut quote: Option<char> = None;
    for ch in inner.chars() {
        match quote {
            Some(active) => {
                buffer.push(ch);
                if ch == active {
                    quote = None;
                }
            }
            None => {
                if (ch == '"' || ch == '\'') && buffer.trim().is_empty() {
                    quote = Some(ch);
                    buffer.push(ch);
                } else if ch == ',' {
                    items.push(std::mem::take(&mut buffer));
                } else {
                    buffer.push(ch);
                }
            }
        }
    }
    items.push(buffer);
    items
}

/// Parity: `coerce` (`frontmatter.ts:186`).
fn coerce(raw: &str) -> Value {
    let value = raw.trim();
    let chars: Vec<char> = value.chars().collect();
    if chars.len() >= 2 {
        let first = chars[0];
        let last = chars[chars.len() - 1];
        if (first == '"' && last == '"') || (first == '\'' && last == '\'') {
            let inner: String = chars[1..chars.len() - 1].iter().collect();
            return Value::String(inner);
        }
    }
    match value {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        "null" | "~" => return Value::Null,
        _ => {}
    }
    if is_integer(value) {
        if let Ok(number) = value.parse::<i64>() {
            return Value::Number(number.into());
        }
        if let Ok(number) = value.parse::<f64>() {
            if let Some(number) = serde_json::Number::from_f64(number) {
                return Value::Number(number);
            }
        }
    }
    if is_decimal(value) {
        if let Ok(number) = value.parse::<f64>() {
            if let Some(number) = serde_json::Number::from_f64(number) {
                return Value::Number(number);
            }
        }
    }
    Value::String(value.to_string())
}

fn is_integer(value: &str) -> bool {
    let digits = value.strip_prefix('-').unwrap_or(value);
    !digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit())
}

fn is_decimal(value: &str) -> bool {
    let unsigned = value.strip_prefix('-').unwrap_or(value);
    let Some((whole, fraction)) = unsigned.split_once('.') else {
        return false;
    };
    whole.chars().all(|ch| ch.is_ascii_digit())
        && !fraction.is_empty()
        && fraction.chars().all(|ch| ch.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_scalars_arrays_and_nested_objects() {
        let parsed = parse_frontmatter(
            "---\nname: hero\ndescription: \"A hero\"\ntriggers:\n  - a\n  - b\nod:\n  mode: deck\n  featured: true\n---\nbody text\n",
        );
        assert_eq!(parsed.data["name"], Value::String("hero".into()));
        assert_eq!(parsed.data["description"], Value::String("A hero".into()));
        assert_eq!(parsed.data["triggers"], serde_json::json!(["a", "b"]));
        assert_eq!(parsed.data["od"]["mode"], Value::String("deck".into()));
        assert_eq!(parsed.data["od"]["featured"], Value::Bool(true));
        assert_eq!(parsed.body, "body text\n");
    }

    #[test]
    fn keeps_flush_left_sequences_under_their_key() {
        let parsed = parse_frontmatter("---\ntriggers:\n- one\n- two\nname: x\n---\n");
        assert_eq!(parsed.data["triggers"], serde_json::json!(["one", "two"]));
        assert_eq!(parsed.data["name"], Value::String("x".into()));
    }

    #[test]
    fn collects_block_scalars_at_their_own_base_indent() {
        let parsed = parse_frontmatter("---\ndescription: |\n  line one\n    indented\n  line three\nname: x\n---\n");
        assert_eq!(
            parsed.data["description"],
            Value::String("line one\n  indented\nline three".into())
        );
    }

    #[test]
    fn splits_inline_arrays_on_top_level_commas_only() {
        let parsed = parse_frontmatter("---\ntags: [\"a,b\", plain, 3]\n---\n");
        assert_eq!(parsed.data["tags"], serde_json::json!(["a,b", "plain", 3]));
    }

    #[test]
    fn keeps_object_array_items() {
        let parsed = parse_frontmatter("---\nitems:\n  - key: one\n    other: two\n---\n");
        assert_eq!(
            parsed.data["items"],
            serde_json::json!([{ "key": "one", "other": "two" }])
        );
    }

    #[test]
    fn a_document_without_a_fence_is_all_body() {
        let parsed = parse_frontmatter("no front matter\n");
        assert!(parsed.data.is_empty());
        assert_eq!(parsed.body, "no front matter\n");
    }
}
