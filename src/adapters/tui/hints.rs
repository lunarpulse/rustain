use crate::adapters::tui::keymap::{Context, Host, render_tips};
use crate::domain::models::FocusState;

/// Returns a registry-derived hint for the base focus, or `None` while a stack
/// frame owns input, hints have faded, or that focus has no hint.
///
/// The base focus is retained under Log/Quit for exact restoration; its bindings
/// must not be advertised until the stack empties. For Input, `is_vscode` selects
/// VS Code keybinding hint or the generic help tip is shown (UX-DR93, AC#4).
/// The `is_vscode` value is read from `TuiState.is_vscode` by the caller (AC9, 3-6a-D2).
// Covers: UX-DR93, UX-DR96, Sprint Change Proposal 2026-04-08
pub fn contextual_hint(
    focus: &FocusState,
    session_count: u32,
    fade_threshold: u32,
    is_vscode: bool,
    stack: &crate::adapters::tui::presentation::ModalStack,
    pending_leader: Option<Context>,
) -> Option<String> {
    if stack.top().is_some() || session_count > fade_threshold {
        return None;
    }
    if pending_leader.is_some() {
        return render_tips::tip(Context::Leader, Host::Local, is_vscode);
    }

    // Story 21.3 A10: the text is generated from the keymap registry rows; only
    // the fade rule and the session count live here.
    match focus {
        FocusState::Input => render_tips::tip(Context::Input, Host::Local, is_vscode),
        FocusState::Chat => render_tips::tip(Context::Chat, Host::Local, is_vscode),
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
    fn palette_hint() -> Option<String> {
        let mut state = crate::adapters::tui::state::TuiState::new(80, 24);
        crate::adapters::tui::app::handle_input(
            &mut state,
            &crate::domain::events::DomainInputEvent::SpecialKey(
                crate::domain::events::DomainKey::CtrlP,
            ),
        );
        contextual_hint(
            &state.focus,
            1,
            5,
            false,
            &state.modal_stack,
            state.pending_leader,
        )
    }

    // ── contextual_hint ──────────────────────────────────────────────────────

    #[test]
    fn test_hint_input_focus() {
        let hint = contextual_hint(&FocusState::Input, 1, 5, false, &Default::default(), None);
        assert!(hint.is_some());
        assert!(hint.unwrap().contains('?'));
    }

    #[test]
    fn test_hint_chat_focus() {
        let hint = contextual_hint(&FocusState::Chat, 1, 5, false, &Default::default(), None);
        assert!(hint.is_some());
        assert!(hint.unwrap().contains("j/k"));
    }

    #[test]
    fn test_hint_fades_when_session_exceeds_threshold() {
        let hint = contextual_hint(&FocusState::Input, 6, 5, false, &Default::default(), None);
        assert!(hint.is_none());
    }

    #[test]
    fn test_hint_at_exact_threshold_shows() {
        let hint = contextual_hint(&FocusState::Input, 5, 5, false, &Default::default(), None);
        assert!(hint.is_some());
    }

    #[test]
    fn test_hint_above_threshold_hidden() {
        let hint = contextual_hint(&FocusState::Input, 6, 5, false, &Default::default(), None);
        assert!(hint.is_none());
    }

    #[test]
    fn test_hint_whichkey_focus() {
        let hint = contextual_hint(
            &FocusState::Input,
            1,
            5,
            false,
            &Default::default(),
            Some(Context::Input),
        );
        assert!(hint.is_some());
        assert!(hint.unwrap().contains("Esc"));
    }

    #[test]
    fn test_hint_command_palette_focus() {
        let hint = palette_hint();
        assert!(
            hint.is_none(),
            "the frame never advertises underlying composer keys"
        );
        assert!(render_tips::palette_tip().contains("filter"));
    }

    // ── VS Code hints (Sprint Change Proposal 2026-04-08, AC#4, AC#5) ─────

    #[test]
    fn test_hint_vscode_input_focus_shows_alt_bindings() {
        let hint = contextual_hint(&FocusState::Input, 1, 5, true, &Default::default(), None);
        assert!(hint.is_some());
        let text = hint.unwrap();
        assert!(
            text.contains("Alt+Enter"),
            "Expected Alt+Enter in hint: {text}"
        );
        assert!(text.contains("Alt+M"), "Expected Alt+M in hint: {text}");
    }

    #[test]
    fn test_hint_vscode_fades_after_threshold() {
        // AC#5: hint disappears after 5 sessions
        let hint = contextual_hint(&FocusState::Input, 6, 5, true, &Default::default(), None);
        assert!(hint.is_none());
    }

    #[test]
    fn test_hint_vscode_shows_at_exact_threshold() {
        let hint = contextual_hint(&FocusState::Input, 5, 5, true, &Default::default(), None);
        assert!(hint.is_some());
    }

    #[test]
    fn test_hint_vscode_chat_focus_shows_generic_hint() {
        // VS Code flag only affects Input focus; Chat focus shows generic hint
        let hint = contextual_hint(&FocusState::Chat, 1, 5, true, &Default::default(), None);
        assert!(hint.is_some());
        assert!(hint.unwrap().contains("j/k"));
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

    // ── Story 21.3 A10: the text is the registry's tip, the fade stays here ──

    #[test]
    fn story21_3_contextual_hint_text_is_projected_from_the_registry() {
        assert_eq!(
            contextual_hint(&FocusState::Input, 1, 5, false, &Default::default(), None),
            render_tips::tip(Context::Input, Host::Local, false)
        );
        assert_eq!(
            contextual_hint(&FocusState::Input, 1, 5, true, &Default::default(), None),
            render_tips::tip(Context::Input, Host::Local, true)
        );
        assert_eq!(
            contextual_hint(&FocusState::Chat, 1, 5, false, &Default::default(), None),
            render_tips::tip(Context::Chat, Host::Local, false)
        );
        let which_key = contextual_hint(
            &FocusState::Input,
            1,
            5,
            false,
            &Default::default(),
            Some(Context::Input),
        );
        let which_key = which_key.unwrap();
        assert!(which_key.contains("Press a key") && which_key.contains("Esc"));
        let palette = palette_hint();
        assert!(
            palette.is_none(),
            "the palette's own registry tip is in its footer"
        );
        let palette = render_tips::palette_tip();
        assert!(palette.contains("Tip:") && palette.contains("filter"));
        let chat =
            contextual_hint(&FocusState::Chat, 1, 5, false, &Default::default(), None).unwrap();
        for key in ["j/k", "J/K", "Tab", "i ", "?"] {
            assert!(chat.contains(key), "{key}: {chat}");
        }
        assert!(!chat.contains("[Enter] Inspect"), "RU-46: {chat}");
    }
}
