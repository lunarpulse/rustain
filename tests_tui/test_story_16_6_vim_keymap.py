"""Story 16.6: Vim Keymap — Fold & Motion.

E2E tests for the vim-inspired keymap: z-prefix chords, bracket chords,
G override, Tab narrow override, and help overlay integration.
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
        tui.assert_screen_contains("G")
        tui.assert_screen_contains("Jump to latest assistant-prose turn")
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
        """G with no conversation falls back safely (empty-transcript guard)."""
        tui.chat_mode()
        tui.send(Chat.JUMP_BOTTOM)
        tui.wait(0.3)
        assert tui.child.isalive(), "TUI process should still be alive after key"


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
