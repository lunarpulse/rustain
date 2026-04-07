use crate::domain::models::FocusState;
use crate::domain::models::visual::OverlayType;

/// Returns a contextual status-bar hint for the given focus state, or `None` when
/// hints have faded (session_count > fade_threshold) or the focus has no hint.
// Covers: UX-DR93, UX-DR96
pub fn contextual_hint(
    focus: &FocusState,
    session_count: u32,
    fade_threshold: u32,
) -> Option<String> {
    if session_count > fade_threshold {
        return None;
    }

    match focus {
        FocusState::Input => {
            Some("Tip: Press ? for help, Ctrl+P for commands".to_string())
        }
        FocusState::Chat => {
            Some("Tip: j/k to scroll, i to type, ? for help".to_string())
        }
        FocusState::Overlay(OverlayType::WhichKey) => {
            Some("Tip: Press a key to execute, Esc to cancel".to_string())
        }
        FocusState::Overlay(OverlayType::CommandPalette) => {
            Some("Tip: Type to filter, Enter to select".to_string())
        }
        _ => None,
    }
}

/// Read the session count from `~/.config/rustain/state.toml`.
/// Returns 0 if the file is missing or unparseable (caller should add 1).
fn read_session_count() -> u32 {
    let path = match crate::infrastructure::paths::config_dir() {
        Ok(dir) => dir.join("state.toml"),
        Err(_) => return 0,
    };
    let contents = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    parse_session_count(&contents).unwrap_or(0)
}

/// Write `session_count` to `~/.config/rustain/state.toml`, preserving other keys.
fn write_session_count(count: u32) {
    let path = match crate::infrastructure::paths::config_dir() {
        Ok(dir) => dir.join("state.toml"),
        Err(_) => return,
    };
    // Read-modify-write: preserve any existing keys in the file
    let mut table = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.parse::<toml::Table>().ok())
        .unwrap_or_default();
    table.insert(
        "session_count".to_string(),
        toml::Value::Integer(count as i64),
    );
    let _ = std::fs::write(&path, table.to_string());
}

/// Parse a `session_count = N` TOML fragment. Returns `None` on parse error.
fn parse_session_count(contents: &str) -> Option<u32> {
    let value: toml::Value = contents.parse().ok()?;
    value
        .get("session_count")?
        .as_integer()
        .and_then(|n| u32::try_from(n).ok())
}

/// Load the persisted session count, increment it, persist the new value, and return it.
/// On any I/O or parse error the count resets to 1.
pub fn load_and_increment_session_count() -> u32 {
    let previous = read_session_count();
    let new_count = previous.saturating_add(1);
    write_session_count(new_count);
    new_count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::FocusState;
    use crate::domain::models::visual::OverlayType;

    // ── contextual_hint ──────────────────────────────────────────────────────

    #[test]
    fn test_hint_input_focus() {
        let hint = contextual_hint(&FocusState::Input, 1, 5);
        assert!(hint.is_some());
        assert!(hint.unwrap().contains('?'));
    }

    #[test]
    fn test_hint_chat_focus() {
        let hint = contextual_hint(&FocusState::Chat, 1, 5);
        assert!(hint.is_some());
        assert!(hint.unwrap().contains("j/k"));
    }

    #[test]
    fn test_hint_fades_when_session_exceeds_threshold() {
        let hint = contextual_hint(&FocusState::Input, 6, 5);
        assert!(hint.is_none());
    }

    #[test]
    fn test_hint_at_exact_threshold_shows() {
        let hint = contextual_hint(&FocusState::Input, 5, 5);
        assert!(hint.is_some());
    }

    #[test]
    fn test_hint_above_threshold_hidden() {
        let hint = contextual_hint(&FocusState::Input, 6, 5);
        assert!(hint.is_none());
    }

    #[test]
    fn test_hint_whichkey_focus() {
        let hint =
            contextual_hint(&FocusState::Overlay(OverlayType::WhichKey), 1, 5);
        assert!(hint.is_some());
        assert!(hint.unwrap().contains("Esc"));
    }

    #[test]
    fn test_hint_command_palette_focus() {
        let hint =
            contextual_hint(&FocusState::Overlay(OverlayType::CommandPalette), 1, 5);
        assert!(hint.is_some());
        assert!(hint.unwrap().contains("filter"));
    }

    // ── parse_session_count ──────────────────────────────────────────────────

    #[test]
    fn test_parse_session_count_valid() {
        assert_eq!(parse_session_count("session_count = 3\n"), Some(3));
    }

    #[test]
    fn test_parse_session_count_zero() {
        assert_eq!(parse_session_count("session_count = 0\n"), Some(0));
    }

    #[test]
    fn test_parse_session_count_invalid() {
        assert_eq!(parse_session_count("not toml @@@"), None);
    }

    #[test]
    fn test_parse_session_count_missing_key() {
        assert_eq!(parse_session_count("other_key = 5\n"), None);
    }
}
