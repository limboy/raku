//! A tool call's arguments, presented as the labeled fields they are.
//!
//! Providers hand arguments over as a JSON object. Shown raw, every call reads
//! as braces and quotes; shown as `Label  value` rows, the few values that
//! matter stand out. The object is parsed once when the activity expands and
//! the caller keeps the result, so a frame only positions rows.

use gpui::SharedString;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct InputField {
    pub(super) label: SharedString,
    pub(super) value: SharedString,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Input {
    pub(super) fields: Vec<InputField>,
    /// Length of the arguments this was built from, so replaced arguments are
    /// rebuilt rather than shown stale.
    pub(super) source_len: usize,
}

/// Fields for a top-level JSON object. Anything else — a bare string, an
/// array, text that is not JSON — has no labels to offer and stays raw.
pub(super) fn build(arguments: &str) -> Option<Input> {
    let value = serde_json::from_str::<serde_json::Value>(arguments).ok()?;
    let object = value.as_object()?;
    if object.is_empty() {
        return None;
    }
    let fields = object
        .iter()
        .map(|(key, value)| InputField {
            label: humanize_key(key).into(),
            value: display_value(value).into(),
        })
        .collect();
    Some(Input {
        fields,
        source_len: arguments.len(),
    })
}

/// Strings read as themselves, without the quotes JSON needs; nested values
/// stay JSON, on one line, since their structure is the information.
fn display_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Null => "null".to_owned(),
        serde_json::Value::Bool(_) | serde_json::Value::Number(_) => value.to_string(),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            serde_json::to_string(value).unwrap_or_default()
        }
    }
}

/// `capture_id` → `Capture ID`, `filePath` → `File path`, `x` → `X`.
fn humanize_key(key: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut previous_lower = false;
    for ch in key.chars() {
        if matches!(ch, '_' | '-' | ' ' | '.') {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            previous_lower = false;
            continue;
        }
        if ch.is_uppercase() && previous_lower && !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
        previous_lower = ch.is_lowercase() || ch.is_ascii_digit();
        current.push(ch);
    }
    if !current.is_empty() {
        words.push(current);
    }
    if words.is_empty() {
        return key.to_owned();
    }
    words
        .iter()
        .enumerate()
        .map(|(index, word)| {
            let lower = word.to_lowercase();
            if word.chars().count() == 1 || ACRONYMS.contains(&lower.as_str()) {
                return word.to_uppercase();
            }
            if index == 0 {
                let mut chars = lower.chars();
                chars
                    .next()
                    .map(|first| first.to_uppercase().chain(chars).collect())
                    .unwrap_or_default()
            } else {
                lower
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

const ACRONYMS: &[&str] = &[
    "id", "ids", "url", "urls", "uri", "api", "mcp", "json", "html", "css", "pid", "ui", "ip",
    "ms", "sql", "http", "https", "ssh", "cwd", "os",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_arguments_become_labeled_fields() {
        let input = build(
            r#"{"x": 1010, "y": 70, "scope": "desktop", "capture_id": "capture_1", "filePath": "/a", "opts": {"deep": true}}"#,
        )
        .expect("fields");
        let rows = input
            .fields
            .iter()
            .map(|field| (field.label.as_ref(), field.value.as_ref()))
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            vec![
                ("X", "1010"),
                ("Y", "70"),
                ("Scope", "desktop"),
                ("Capture ID", "capture_1"),
                ("File path", "/a"),
                ("Opts", r#"{"deep":true}"#),
            ]
        );
    }

    #[test]
    fn non_objects_stay_raw() {
        assert!(build("ls -la").is_none());
        assert!(build("[1, 2]").is_none());
        assert!(build("{}").is_none());
    }
}
