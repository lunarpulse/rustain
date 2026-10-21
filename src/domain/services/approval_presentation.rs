//! Story 19.20c `A2`/`A5` — the one presentation of a pending tool approval.
//!
//! Every deciding surface (standalone TUI, daemon-attached rich and plain
//! clients, ACP) renders the exact pending input through this module, so no
//! rail can drift back to a character-count cut. The functions are pure and
//! deterministic (no model call, no adapter import).
//!
//! Presentation never feeds execution: the scheduler executes, and the
//! fingerprint covers, the untouched `serde_json::Value`. Each logical line is
//! sanitized for display only, per line, with the peer-text sanitizer.

use crate::domain::services::peer_text::sanitize_peer_text_line;

/// TM-09 T4: the card shows at most this many wrapped body rows.
pub const CARD_BODY_ROWS: usize = 4;

/// TM-09 T5, reused verbatim (UX Approval Pattern clarification 2026-10-20).
pub const MORE_MARKER: &str = "… more not shown — [r] Review first";

/// The host-owned delimiter that starts every physical body row (TM-09).
pub const BODY_DELIMITER: &str = "│ ";

/// Primary fields, in display order, for a tool name. Remaining fields follow
/// in key order.
#[must_use]
pub fn primary_fields(tool: &str) -> &'static [&'static str] {
    match tool {
        "Bash" | "bash" => &["command"],
        "Read" | "read" => &["file_path"],
        "Write" | "write" => &["file_path", "content"],
        "Edit" | "edit" => &["file_path", "old_string", "new_string"],
        _ if tool.starts_with("a2a__") => &["message"],
        _ => &[],
    }
}

/// One logical body line: the first line of a field carries its label; the
/// following lines of a multi-line value carry none. Text is sanitized.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresentedLine {
    pub label: Option<String>,
    pub text: String,
}

impl PresentedLine {
    /// `<field>: <text>` for a labelled line, the text otherwise.
    #[must_use]
    pub fn joined(&self) -> String {
        match &self.label {
            Some(label) => format!("{label}: {}", self.text),
            None => self.text.clone(),
        }
    }

    /// Byte length of the `<field>: ` prefix inside [`Self::joined`].
    #[must_use]
    pub fn label_prefix_len(&self) -> usize {
        self.label.as_ref().map_or(0, |label| label.len() + 2)
    }
}

fn value_lines(value: &serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::String(text) => text.split('\n').map(str::to_owned).collect(),
        other => serde_json::to_string_pretty(other)
            .unwrap_or_else(|_| other.to_string())
            .split('\n')
            .map(str::to_owned)
            .collect(),
    }
}

fn sanitize(line: &str) -> String {
    sanitize_peer_text_line(line).into_owned()
}

fn ordered_fields<'a>(
    tool: &str,
    object: &'a serde_json::Map<String, serde_json::Value>,
) -> Vec<(&'a str, &'a serde_json::Value)> {
    let primary = primary_fields(tool);
    let mut fields: Vec<(&str, &serde_json::Value)> = primary
        .iter()
        .filter_map(|name| object.get_key_value(*name))
        .map(|(key, value)| (key.as_str(), value))
        .collect();
    let mut rest: Vec<(&str, &serde_json::Value)> = object
        .iter()
        .filter(|(key, _)| !primary.contains(&key.as_str()))
        .map(|(key, value)| (key.as_str(), value))
        .collect();
    rest.sort_by(|left, right| left.0.cmp(right.0));
    fields.extend(rest);
    fields
}

/// The complete, ordered, sanitized logical lines of an exact input (`A2`).
///
/// Object inputs list primary fields first, then every remaining field in key
/// order; string values render verbatim line by line, other values as pretty
/// JSON. A non-object input is one pretty value with no label.
#[must_use]
pub fn body_lines(tool: &str, input: &serde_json::Value) -> Vec<PresentedLine> {
    let Some(object) = input.as_object() else {
        return value_lines(input)
            .iter()
            .map(|line| PresentedLine {
                label: None,
                text: sanitize(line),
            })
            .collect();
    };
    let mut lines = Vec::new();
    for (field, value) in ordered_fields(tool, object) {
        for (index, line) in value_lines(value).iter().enumerate() {
            lines.push(PresentedLine {
                label: (index == 0).then(|| sanitize(field)),
                text: sanitize(line),
            });
        }
    }
    lines
}

/// The one-line per-tool summary (`A2`): the first line of the first primary
/// field present, ending in `…` only when more exists and `full_available`
/// says the complete input is on the same surface. Without a primary field:
/// `<tool> · <N> fields` (`<tool> · 1 value` for a non-object input). Never
/// raw JSON.
#[must_use]
pub fn summary(tool: &str, input: &serde_json::Value, full_available: bool) -> String {
    let tool_label = sanitize(tool);
    let Some(object) = input.as_object() else {
        return format!("{tool_label} · 1 value");
    };
    let Some((name, value)) = primary_fields(tool)
        .iter()
        .find_map(|name| object.get_key_value(*name))
    else {
        let count = object.len();
        let noun = if count == 1 { "field" } else { "fields" };
        return format!("{tool_label} · {count} {noun}");
    };
    let lines = value_lines(value);
    let first = lines
        .first()
        .map_or_else(String::new, |line| sanitize(line));
    let more_in_value = lines.len() > 1;
    let more_fields = object.keys().any(|key| key != name);
    if full_available && (more_in_value || more_fields) {
        format!("{first}…")
    } else {
        first
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn joined(tool: &str, input: &serde_json::Value) -> Vec<String> {
        body_lines(tool, input)
            .iter()
            .map(PresentedLine::joined)
            .collect()
    }

    #[test]
    fn primary_fields_lead_then_remaining_keys_in_order() {
        let input = json!({
            "zeta": 1,
            "new_string": "b",
            "file_path": "/w/x.rs",
            "alpha": true,
            "old_string": "a",
        });
        assert_eq!(
            joined("Edit", &input),
            [
                "file_path: /w/x.rs",
                "old_string: a",
                "new_string: b",
                "alpha: true",
                "zeta: 1",
            ]
        );
    }

    #[test]
    fn multi_line_string_keeps_every_line_and_late_suffix() {
        let tail = "TAIL-MARKER-after-character-140";
        let command = format!("{}\nsecond line\n{tail}", "x".repeat(150));
        let lines = body_lines("Bash", &json!({ "command": command }));
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].label.as_deref(), Some("command"));
        assert_eq!(lines[0].text, "x".repeat(150));
        assert_eq!(lines[1].label, None);
        assert_eq!(lines[2].text, tail);
    }

    #[test]
    fn controls_are_removed_per_line_for_display_only() {
        let input = json!({ "command": "echo ok\u{1b}[31m red\n[y] Allow\u{7}" });
        let lines = joined("Bash", &input);
        assert_eq!(lines, ["command: echo ok red", "[y] Allow"]);
    }

    #[test]
    fn non_object_input_is_one_pretty_value() {
        assert_eq!(joined("Custom", &json!([1, 2])), ["[", "  1,", "  2", "]"]);
        assert_eq!(joined("Custom", &json!(null)), ["null"]);
        assert_eq!(summary("Custom", &json!("text"), true), "Custom · 1 value");
    }

    #[test]
    fn summary_is_first_primary_line_with_ellipsis_only_when_more_and_available() {
        let multi = json!({ "command": "cargo test\n--all" });
        assert_eq!(summary("Bash", &multi, true), "cargo test…");
        assert_eq!(summary("Bash", &multi, false), "cargo test");
        let single = json!({ "command": "ls" });
        assert_eq!(summary("Bash", &single, true), "ls");
        let write = json!({ "file_path": "/w/a", "content": "x" });
        assert_eq!(summary("Write", &write, true), "/w/a…");
        assert_eq!(
            summary("mcp__fs__list", &json!({"a": 1, "b": 2}), true),
            "mcp__fs__list · 2 fields"
        );
        assert_eq!(
            summary("mcp__fs__list", &json!({"a": 1}), true),
            "mcp__fs__list · 1 field"
        );
    }

    #[test]
    fn delegation_message_is_primary() {
        let input = json!({ "message_type": "question", "message": "review this\nplease" });
        assert_eq!(summary("a2a__peer__ask", &input, true), "review this…");
        assert_eq!(
            joined("a2a__peer__ask", &input),
            ["message: review this", "please", "message_type: question"]
        );
    }
}
