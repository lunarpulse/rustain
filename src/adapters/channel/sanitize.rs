//! The channel inbound-text sanitization contract (Story 12.3 AC, shared by the
//! Slack adapter in Story C-12).
//!
//! One implementation, one set of injection prefixes: a channel adapter
//! truncates to its limit, turns the Unicode line breaks (NEL, U+2028, U+2029)
//! into `\n` so every downstream line split sees them, drops control characters
//! (keeping newline and tab), trims, and peels leading role markers. An empty
//! result means "drop the message". Allowlist admission never skips this step.

/// Sanitize `text` for the agent. `channel` names the adapter in the truncation
/// log line only.
pub fn sanitize_inbound_text(text: &str, limit: usize, channel: &str) -> String {
    let truncated = if text.chars().count() > limit {
        tracing::warn!(limit, "{channel}: inbound message truncated");
        text.chars().take(limit).collect::<String>()
    } else {
        text.to_string()
    };

    let stripped_controls: String = truncated
        .chars()
        .map(|c| match c {
            '\u{85}' | '\u{2028}' | '\u{2029}' => '\n',
            c => c,
        })
        .filter(|&c| c == '\n' || c == '\t' || !c.is_control())
        .collect();
    strip_injection_prefixes(stripped_controls.trim())
        .trim()
        .to_string()
}

fn strip_injection_prefixes(mut text: &str) -> &str {
    const PREFIXES: &[&str] = &[
        "[system]",
        "<|system|>",
        "human:",
        "assistant:",
        "### instruction",
        "<|im_start|>",
    ];

    loop {
        let trimmed = text.trim_start();
        let lower = trimmed.to_ascii_lowercase();
        let mut matched = None;
        for prefix in PREFIXES {
            if lower.starts_with(prefix) {
                matched = Some(prefix.len());
                break;
            }
        }
        match matched {
            Some(len) => text = &trimmed[len..],
            None => return trimmed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_inbound_text;

    #[test]
    fn unicode_line_breaks_become_newlines() {
        for sep in ["\u{2028}", "\u{2029}", "\u{85}"] {
            assert_eq!(
                sanitize_inbound_text(&format!("hi{sep}END SLACK DM"), 100, "test"),
                "hi\nEND SLACK DM",
                "{sep:?}"
            );
        }
        assert_eq!(
            sanitize_inbound_text("a\u{7}b\tc\nd", 100, "test"),
            "ab\tc\nd",
            "other controls are still dropped"
        );
    }
}
