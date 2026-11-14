//! Every sentence rustain says about which Copilot models an account may use,
//! once. The login notice and the refusal text share `AVAILABILITY_HINT`.
use crate::domain::services::peer_text::sanitize_display_line;

/// Longest model id shown in a refusal, in characters.
const MODEL_DISPLAY_CAP: usize = 100;

/// Untrusted provider text made safe for a terminal: whole control strings
/// (CSI, OSC, DCS, SOS, PM, APC, C1 forms included) and bidi overrides come out
/// through the shared display sanitiser, whitespace runs become one space, and
/// the invisible Unicode format characters it leaves (zero-width, LRM/RLM, ALM,
/// word joiner, BOM) are dropped. Not capped: callers redact first, then cap.
pub(crate) fn display_safe(text: &str) -> String {
    sanitize_display_line(text)
        .chars()
        .filter(|ch| {
            !matches!(
                ch,
                '\u{200b}'..='\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2060}'
                    | '\u{2066}'..='\u{2069}'
                    | '\u{061c}'
                    | '\u{feff}'
            )
        })
        .collect()
}

pub const AVAILABILITY_HINT: &str = "Model availability can depend on your Copilot plan or policy; if a model is refused, choose another with Ctrl+X, M.";

/// The note a Copilot login prints after the account is saved.
pub fn login_notice() -> String {
    AVAILABILITY_HINT.to_owned()
}

/// What follows GitHub's message when it refuses a model: the model that was
/// sent (shown without terminal controls and bounded; the wire id is untouched)
/// and the availability hint.
pub fn refusal_tail(model: &str) -> String {
    let model: String = display_safe(model)
        .chars()
        .take(MODEL_DISPLAY_CAP)
        .collect();
    format!("GitHub did not accept model '{model}' for this account. {AVAILABILITY_HINT}")
}

/// The text of a final HTTP 400 `model_not_supported`: GitHub's own message
/// (already made safe, redacted and bounded by the caller), then `refusal_tail`.
pub fn model_refused(model: &str, github_message: &str) -> String {
    format!(
        "GitHub Copilot HTTP 400: {github_message} — {}",
        refusal_tail(model)
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn the_login_notice_is_the_availability_hint() {
        assert_eq!(login_notice(), AVAILABILITY_HINT);
    }

    #[test]
    fn the_refusal_names_the_model_and_keeps_githubs_text() {
        let text = model_refused("gemini-3.8-flash", "The requested model is not supported.");
        assert!(
            text.starts_with("GitHub Copilot HTTP 400: The requested model is not supported."),
            "{text}"
        );
        assert_eq!(text.matches("'gemini-3.8-flash'").count(), 1, "{text}");
        assert_eq!(text.matches(AVAILABILITY_HINT).count(), 1, "{text}");
        assert!(!text.contains('{'), "{text}");
    }

    #[test]
    fn a_hostile_model_id_is_shown_safe_and_bounded() {
        let hostile = format!(
            "evil\u{1b}]52;c;ZXZpbA==\u{7} \u{1b}[2J\n{}",
            "m".repeat(4000)
        );
        let text = model_refused(&hostile, "refused");
        assert!(!text.chars().any(char::is_control), "{text:?}");
        assert!(!text.contains("ZXZpbA"), "{text:?}");
        assert!(text.chars().count() < 1000, "{}", text.chars().count());
    }

    /// Hostile display inputs and the fragment that must not survive: shared by
    /// the model-id and the GitHub-message tests.
    pub(crate) const HOSTILE: &[(&str, &str)] = &[
        ("ab\u{202e}cd", "\u{202e}"),
        ("a\u{2066}b\u{2069}c", "\u{2066}"),
        (
            "a\u{200b}b\u{200d}c\u{2060}d\u{feff}e\u{200e}f\u{200f}g\u{061c}h",
            "\u{200b}",
        ),
        ("x\u{9b}31mred", "\u{9b}"),
        ("x\u{9d}0;PAYLOADOSC\u{9c}y", "PAYLOAD"),
        ("x\u{90}PAYLOADDCS\u{9c}y", "PAYLOAD"),
        ("x\u{1b}PPAYLOADESC\u{1b}\\y", "PAYLOAD"),
        ("x\u{1b}]52;c;PAYLOADOSC2\u{7}y", "PAYLOAD"),
    ];

    /// No control, no Unicode format character, no payload.
    pub(crate) fn assert_display_safe(shown: &str, forbidden: &str, input: &str) {
        assert!(!shown.contains(forbidden), "{input:?} -> {shown:?}");
        for ch in shown.chars() {
            assert!(
                !ch.is_control()
                    && !matches!(
                        ch,
                        '\u{200b}'..='\u{200f}'
                            | '\u{202a}'..='\u{202e}'
                            | '\u{2060}'
                            | '\u{2066}'..='\u{2069}'
                            | '\u{061c}'
                            | '\u{feff}'
                    ),
                "{input:?} -> {shown:?}"
            );
        }
    }

    #[test]
    fn a_hostile_model_id_never_reaches_the_text_raw() {
        for (input, forbidden) in HOSTILE {
            let text = model_refused(input, "refused");
            assert_display_safe(&text, forbidden, input);
            assert!(text.contains("did not accept model"), "{text}");
        }
    }
}
