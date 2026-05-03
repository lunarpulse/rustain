"""Story 16.6: Vim Keymap — Fold & Motion.

E2E tests for the vim-inspired keymap: z-prefix chords, bracket chords
(]] / [[ / ]P), Tab narrow override, and help overlay integration.

Note (S16.8 preflight rebinding 2026-05-03): The S16.6 G→JumpToLatestProseAnchor
binding moved to the `]P` bracket-prefix chord (NOT a `gp` g-prefix chord — `gp`
would have caused a TOP→BOTTOM flicker because Story 1.4's single-`g`=jump-to-top
fires on the first keystroke; bracket-prefix has no such side effect). S16.8 can
now return G to vim-bottom semantic. The dispatcher arm at
event_loop.rs::JumpToLatestProseAnchor is unchanged — only the keystroke that
produces the action moved. See ADR-16-03 for the "anchor as explicit user
investment" principle that motivated the rebinding.
"""

import pytest

from harness import RustainTUI
from keys import ESC, ENTER, TAB, Chat, Vim


# ── API-free tests: help overlay & chord state machines ──────────────────────

@pytest.mark.story_16_6
class TestVimHelpOverlay:
    """Help overlay lists vim bindings (AC8)."""

    def test_help_shows_vim_fold_and_motion_category(self, tui: RustainTUI):
        """Help overlay contains 'VIM FOLD & MOTION' with expected bindings."""
        tui.chat_mode()
        tui.open_help()
        tui.wait_for_screen("VIM FOLD & MOTION")
        tui.assert_screen_contains("VIM FOLD & MOTION")
        # Spot-check key bindings from AC8 table
        tui.assert_screen_contains("za")
        tui.assert_screen_contains("Toggle fold on focused turn")
        tui.assert_screen_contains("zM")
        tui.assert_screen_contains("Collapse all turns")
        tui.assert_screen_contains("zs")
        tui.assert_screen_contains("Toggle summary tier")
        tui.assert_screen_contains("zz")
        tui.assert_screen_contains("Recenter view")
        tui.assert_screen_contains("]]")
        tui.assert_screen_contains("Jump to next assistant-prose turn")
        tui.assert_screen_contains("Tab")
        tui.assert_screen_contains("Cycle invocations")
        # S16.8 rebinding: jump-to-latest-prose moved from G to ]P bracket-prefix chord
        # per ADR-16-03 (chosen over gp to avoid the single-g jump-to-top flicker)
        tui.assert_screen_contains("]P")
        tui.assert_screen_contains("Jump to latest assistant-prose turn")
        # G now reads as vim-bottom (restored by S16.8)
        tui.assert_screen_contains("G")
        tui.assert_screen_contains("Jump to bottom")
        tui.close_overlay()


@pytest.mark.story_16_6
class TestVimChordStateMachines:
    """z-prefix and bracket-prefix chord state machines (AC10)."""

    def test_z_prefix_sets_pending_and_dispatches(self, tui: RustainTUI):
        """z + a dispatches FoldToggleAtFocus without crashing."""
        tui.chat_mode()
        tui.send(Vim.Z_LEADER)
        tui.wait(0.2)
        tui.send("a")
        tui.wait(0.3)
        assert tui.child.isalive(), "TUI process should still be alive after key"

    def test_z_prefix_invalid_chord_consumes_legacy_key(self, tui: RustainTUI):
        """z + j is consumed as cancelled chord — 'j' does NOT scroll."""
        tui.chat_mode()
        screen_before = tui.get_screen_text()
        tui.send(Vim.Z_LEADER)
        tui.wait(0.2)
        tui.send(Chat.SCROLL_DOWN)
        tui.wait(0.3)
        screen_after = tui.get_screen_text()
        # The chord is consumed, so no scroll happens → screen unchanged
        assert screen_after == screen_before, (
            "z+j should be consumed as cancelled chord; screen should not change"
        )

    def test_bracket_prefix_dispatches_jump(self, tui: RustainTUI):
        """] + ] dispenses JumpProseAnchor without crashing."""
        tui.chat_mode()
        tui.send("]")
        tui.wait(0.2)
        tui.send("]")
        tui.wait(0.3)
        assert tui.child.isalive(), "TUI process should still be alive after key"

    def test_bracket_prefix_invalid_chord_consumes_legacy_key(self, tui: RustainTUI):
        """] + a is consumed as cancelled chord."""
        tui.chat_mode()
        screen_before = tui.get_screen_text()
        tui.send("]")
        tui.wait(0.2)
        tui.send("a")
        tui.wait(0.3)
        screen_after = tui.get_screen_text()
        assert screen_after == screen_before, (
            "]+a should be consumed as cancelled chord; screen should not change"
        )

    def test_esc_cancels_z_chord(self, tui: RustainTUI):
        """ESC after 'z' resets pending_z so subsequent 'j' scrolls normally."""
        tui.chat_mode()
        tui.send(Vim.Z_LEADER)
        tui.wait(0.2)
        tui.send(ESC)
        tui.wait(0.3)
        screen_before = tui.get_screen_text()
        tui.send(Chat.SCROLL_DOWN)
        tui.wait(0.3)
        screen_after = tui.get_screen_text()
        assert screen_after != screen_before, (
            "j should scroll after ESC cancels z chord"
        )

    def test_esc_cancels_bracket_chord(self, tui: RustainTUI):
        """ESC after ']' resets pending_bracket so subsequent 'j' scrolls."""
        tui.chat_mode()
        tui.send("]")
        tui.wait(0.2)
        tui.send(ESC)
        tui.wait(0.3)
        screen_before = tui.get_screen_text()
        tui.send(Chat.SCROLL_DOWN)
        tui.wait(0.3)
        screen_after = tui.get_screen_text()
        assert screen_after != screen_before, (
            "j should scroll after ESC cancels bracket chord"
        )

    def test_g_capital_in_empty_chat_no_crash(self, tui: RustainTUI):
        """G with no conversation falls back safely (empty-transcript guard).

        Post-S16.8 rebinding (2026-05-03): G now means vim-bottom (legacy 2-mode
        scroll_offset=0 + auto_scroll=true) rather than JumpToLatestProseAnchor.
        Either binding must not crash on an empty transcript — the smoke test
        invariant is unchanged.
        """
        tui.chat_mode()
        tui.send(Chat.JUMP_BOTTOM)
        tui.wait(0.3)
        assert tui.child.isalive(), "TUI process should still be alive after key"

    def test_rbracket_capital_p_chord_in_empty_chat_no_crash(self, tui: RustainTUI):
        """]P chord with no conversation falls back safely (empty-transcript guard).

        S16.8 preflight rebinding (2026-05-03): the JumpToLatestProseAnchor
        behavior moved from G to the ]P bracket-prefix chord per ADR-16-03 (chosen
        over gp to avoid the single-g jump-to-top flicker — bracket leader has no
        first-key side effect). The event_loop.rs::JumpToLatestProseAnchor
        dispatcher arm is unchanged; only the keystroke that produces it moved.
        """
        tui.chat_mode()
        tui.send("]")           # arm pending_bracket = Some(']')
        tui.wait(0.2)
        tui.send("P")           # complete ]P chord (capital P)
        tui.wait(0.3)
        assert tui.child.isalive(), "TUI process should still be alive after ]P chord"

    def test_rbracket_lowercase_p_does_not_dispatch(self, tui: RustainTUI):
        """] + lowercase p (not the binding) is consumed; pending_bracket cleared.

        Only ]P (capital P) dispatches JumpToLatestProseAnchor. Lowercase p falls
        through the chord handler's catch-all → Consumed. Prevents accidental
        dispatch on shift-key fumble.
        """
        tui.chat_mode()
        tui.send("]")
        tui.wait(0.2)
        tui.send("p")  # lowercase — should NOT dispatch
        tui.wait(0.3)
        assert tui.child.isalive(), "TUI process should still be alive after ]p"


# ── API-required tests: motions with actual conversation turns ───────────────

@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_16_6
class TestVimMotionsWithConversation:
    """Vim motions that require a conversation with at least one assistant turn."""

    @pytest.fixture(autouse=True)
    def _seed_conversation(self, tui: RustainTUI):
        """Send a message and wait for reply before each test."""
        tui.send_message("Say hello")
        tui.wait_for_idle()
        tui.chat_mode()
        # Extra settle time so the committed turn is drained from reducer
        # into conversation.turns before we exercise motion keys.
        tui.wait(1.0)

    def test_bracket_jump_next_and_prev(self, tui: RustainTUI):
        """]] and [[ jump between assistant-prose turns (AC6).

        With a single turn, ]] reports "no prose anchor in direction=Down"
        and [[ reports "no prose anchor in direction=Up" — both prove the
        handlers were reached and searched correctly.
        """
        tui.send("]")
        tui.wait(0.2)
        tui.send("]")
        tui.wait(0.5)
        tui.assert_log_contains("JumpProseAnchor: no prose anchor in direction=Down")

        tui.send("[")
        tui.wait(0.2)
        tui.send("[")
        tui.wait(0.5)
        tui.assert_log_contains("JumpProseAnchor: no prose anchor in direction=Up")

    def test_tab_in_chat_produces_cycle_log(self, tui: RustainTUI):
        """Tab in Chat focus emits CycleInvocationInFocusedTurn (AC5).

        With a prose-only turn the guard fails and falls through, but the
        dispatcher is still reached — verified via log.
        """
        tui.send(TAB)
        tui.wait(0.5)
        tui.assert_log_contains("CycleInvocationInFocusedTurn: dispatching")
