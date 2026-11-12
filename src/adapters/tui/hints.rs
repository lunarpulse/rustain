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
    stage_target: bool,
) -> Option<String> {
    if stack.top().is_some() || session_count > fade_threshold {
        return None;
    }
    if pending_leader.is_some() {
        return render_tips::tip(Context::Leader, Host::Local, is_vscode, false);
    }

    // Story 21.3 A10: the text is generated from the keymap registry rows; only
    // the fade rule and the session count live here.
    match focus {
        FocusState::Input => render_tips::tip(Context::Input, Host::Local, is_vscode, false),
        FocusState::Chat => render_tips::tip(Context::Chat, Host::Local, is_vscode, stage_target),
    }
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
            false,
        )
    }

    // ── contextual_hint ──────────────────────────────────────────────────────

    #[test]
    fn test_hint_input_focus() {
        let hint = contextual_hint(
            &FocusState::Input,
            1,
            5,
            false,
            &Default::default(),
            None,
            false,
        );
        assert!(hint.is_some());
        assert!(hint.unwrap().contains('?'));
    }

    #[test]
    fn test_hint_chat_focus() {
        let hint = contextual_hint(
            &FocusState::Chat,
            1,
            5,
            false,
            &Default::default(),
            None,
            false,
        );
        assert!(hint.is_some());
        assert!(hint.unwrap().contains("j/k"));
    }

    #[test]
    fn test_hint_fades_when_session_exceeds_threshold() {
        let hint = contextual_hint(
            &FocusState::Input,
            6,
            5,
            false,
            &Default::default(),
            None,
            false,
        );
        assert!(hint.is_none());
    }

    #[test]
    fn test_hint_at_exact_threshold_shows() {
        let hint = contextual_hint(
            &FocusState::Input,
            5,
            5,
            false,
            &Default::default(),
            None,
            false,
        );
        assert!(hint.is_some());
    }

    #[test]
    fn test_hint_above_threshold_hidden() {
        let hint = contextual_hint(
            &FocusState::Input,
            6,
            5,
            false,
            &Default::default(),
            None,
            false,
        );
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
            false,
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
        let hint = contextual_hint(
            &FocusState::Input,
            1,
            5,
            true,
            &Default::default(),
            None,
            false,
        );
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
        let hint = contextual_hint(
            &FocusState::Input,
            6,
            5,
            true,
            &Default::default(),
            None,
            false,
        );
        assert!(hint.is_none());
    }

    #[test]
    fn test_hint_vscode_shows_at_exact_threshold() {
        let hint = contextual_hint(
            &FocusState::Input,
            5,
            5,
            true,
            &Default::default(),
            None,
            false,
        );
        assert!(hint.is_some());
    }

    #[test]
    fn test_hint_vscode_chat_focus_shows_generic_hint() {
        // VS Code flag only affects Input focus; Chat focus shows generic hint
        let hint = contextual_hint(
            &FocusState::Chat,
            1,
            5,
            true,
            &Default::default(),
            None,
            false,
        );
        assert!(hint.is_some());
        assert!(hint.unwrap().contains("j/k"));
    }

    // ── Story 21.3 A10: the text is the registry's tip, the fade stays here ──

    #[test]
    fn story21_3_contextual_hint_text_is_projected_from_the_registry() {
        assert_eq!(
            contextual_hint(
                &FocusState::Input,
                1,
                5,
                false,
                &Default::default(),
                None,
                false
            ),
            render_tips::tip(Context::Input, Host::Local, false, false)
        );
        assert_eq!(
            contextual_hint(
                &FocusState::Input,
                1,
                5,
                true,
                &Default::default(),
                None,
                false
            ),
            render_tips::tip(Context::Input, Host::Local, true, false)
        );
        assert_eq!(
            contextual_hint(
                &FocusState::Chat,
                1,
                5,
                false,
                &Default::default(),
                None,
                false
            ),
            render_tips::tip(Context::Chat, Host::Local, false, false)
        );
        let which_key = contextual_hint(
            &FocusState::Input,
            1,
            5,
            false,
            &Default::default(),
            Some(Context::Input),
            false,
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
        let chat = contextual_hint(
            &FocusState::Chat,
            1,
            5,
            false,
            &Default::default(),
            None,
            false,
        )
        .unwrap();
        for key in ["j/k", "J/K", "Tab", "i ", "?"] {
            assert!(chat.contains(key), "{key}: {chat}");
        }
        assert!(!chat.contains("[Enter] Inspect"), "RU-46: {chat}");
    }

    #[test]
    fn story21_5_inspect_tip_requires_the_available_chat_target() {
        let stack = Default::default();
        let expected = format!(
            "[{}] Inspect",
            crate::adapters::tui::keymap::Key::Enter.label()
        );
        let chat = contextual_hint(&FocusState::Chat, 1, 5, false, &stack, None, true).unwrap();
        assert!(chat.contains(&expected), "{chat}");
        for (focus, target) in [(FocusState::Input, true), (FocusState::Chat, false)] {
            let hint = contextual_hint(&focus, 1, 5, false, &stack, None, target).unwrap();
            assert!(!hint.contains(&expected), "{hint}");
        }
        let mut state = crate::adapters::tui::state::TuiState::new(80, 24);
        crate::adapters::tui::presentation::reducer::open(
            &mut state,
            crate::adapters::tui::presentation::Surface::Help,
        );
        assert!(
            contextual_hint(
                &FocusState::Chat,
                1,
                5,
                false,
                &state.modal_stack,
                None,
                true
            )
            .is_none()
        );
        let attached = render_tips::tip(Context::Chat, Host::Attached, false, true).unwrap();
        assert!(!attached.contains(&expected), "{attached}");
    }
}
