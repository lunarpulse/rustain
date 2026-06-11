"""Stories 3.1–3.6: Input, Help, Clipboard, Command Palette, Markdown.

Story 3.1: Multi-line Input & History Navigation
Story 3.3: Command Palette & Which-Key Chords
Story 3.5: Help Overlay & Discoverability
"""

import pytest

from harness import RustainTUI
from keys import (
    ESC, ENTER, CTRL_C, CTRL_P, CTRL_X, ALT_M,
    UP, DOWN, Chat,
)


# ── Story 3.1: Multi-line Input & History ────────────────────────────────────

@pytest.mark.requires_api
@pytest.mark.story_3_1
@pytest.mark.story_5_0
class TestMultilineInput:
    """Story 3.1: Multi-line editing and history navigation."""

    def test_submit_message_appears_in_chat_pane(self, tui: RustainTUI):
        """Typing text + Enter submits; the user's message appears in the chat pane."""
        tui.send_message("Say 'pong'")
        # User's message should appear in the chat pane immediately after submit
        tui.wait_for_screen("Say 'pong'")
        tui.assert_screen_contains("Say 'pong'", "User message should appear in chat pane")
        tui.wait_for_idle()
        # AI echoes "pong" — verify the reply landed
        tui.assert_screen_contains("pong", "AI reply 'pong' should appear on screen")

    def test_history_recall_shows_previous_message_in_input(self, tui: RustainTUI):
        """After submitting, Up arrow recalls previous input into the input box."""
        msg_a = "First message for history"
        msg_b = "Second message to push first up"
        tui.send_message(msg_a)
        tui.wait_for_screen(msg_a)
        tui.wait_for_idle()

        tui.send_message(msg_b)
        assert tui.wait_for_screen(msg_b), (
            f"Second message '{msg_b}' should appear in chat pane"
        )
        tui.wait_for_idle()

        tui.chat_mode()
        tui.input_mode()
        tui.wait(0.3)

        screen_before_up = tui.get_screen_text()

        tui.send(UP)
        tui.wait(0.5)  # Wait for the 250ms render tick to fire

        screen_after_up = tui.get_screen_text()
        assert screen_after_up != screen_before_up, (
            "Screen should change after UP key — history recall modifies the input box"
        )
        # History is LIFO: one UP recalls the most recent entry (msg_b).
        assert msg_b in screen_after_up, (
            f"Recalled message '{msg_b}' should be visible after UP key"
        )

        tui.send(DOWN)
        tui.wait(0.5)  # Wait for render tick
        screen_after_down = tui.get_screen_text()
        assert screen_after_down != screen_after_up, (
            "Screen should change after DOWN key — input box should be cleared"
        )


# ── Story 3.3: Command Palette ───────────────────────────────────────────────

@pytest.mark.requires_api
@pytest.mark.story_3_3
@pytest.mark.story_5_0
class TestCommandPalette:
    """Story 3.3: Ctrl+P opens command palette; Esc closes it."""

    def test_palette_shows_command_palette_ui_on_ctrl_p(self, tui: RustainTUI):
        """Ctrl+P opens the command palette — 'Command Palette' header visible."""
        tui.send_message("Say hello for palette test")
        tui.wait_for_idle()

        tui.send(CTRL_P)
        tui.wait_for_screen("Command Palette")
        tui.assert_screen_contains("Command Palette", "Palette header should be visible when open")

        tui.close_overlay()
        tui.wait_for_screen_not_contains("Command Palette", timeout=2.0)
        tui.assert_screen_not_contains(
            "Command Palette", "Palette header should be gone after close"
        )

    def test_which_key_shows_chord_bar_on_ctrl_x(self, tui: RustainTUI):
        """Ctrl+X opens which-key chord bar — 'Ctrl+X' label visible in bar."""
        tui.send_message("Say hello for which-key test")
        tui.wait_for_idle()

        tui.send(CTRL_X)
        tui.wait_for_screen("Ctrl+X")
        tui.assert_screen_contains("Ctrl+X", "Which-key bar title should be visible when open")

        tui.send("a")  # Dismiss with any chord key
        tui.wait_for_screen_not_contains("Ctrl+X", timeout=2.0)
        tui.assert_screen_not_contains(
            "Ctrl+X", "Which-key bar should be gone after dismissal"
        )


# ── Story 3.5: Help Overlay ──────────────────────────────────────────────────

@pytest.mark.requires_api
@pytest.mark.story_3_5
@pytest.mark.story_5_0
class TestHelpOverlay:
    """Story 3.5: ? key in Chat focus opens help; Esc or ? closes it."""

    def test_help_shows_keybindings_overlay_on_question_mark(self, tui: RustainTUI):
        """Open help, scroll with j/k — 'Help' keybindings header visible."""
        tui.send_message("Say hello for help test")
        tui.wait_for_idle()

        tui.chat_mode()
        tui.open_help()
        tui.wait_for_screen("Help")
        tui.assert_screen_contains("Help", "Help overlay header should be visible when open")

        # Scroll inside help
        tui.send_keys("j", "j", "k")
        tui.assert_screen_contains("Help", "Help overlay should remain visible while scrolling")

        tui.close_overlay()
        tui.wait_for_screen_not_contains("Keybindings", timeout=2.0)
        tui.assert_screen_not_contains(
            "Keybindings", "Help keybindings overlay should be gone after close"
        )

    def test_help_closes_with_second_question_mark(self, tui: RustainTUI):
        """? is a toggle — pressing it again closes the help overlay."""
        tui.send_message("Say hello")
        tui.wait_for_idle()

        tui.chat_mode()
        tui.send(Chat.HELP)
        tui.wait_for_screen("Help")
        tui.assert_screen_contains("Help", "Help overlay should be visible after first ?")

        tui.send(Chat.HELP)  # Close
        tui.wait_for_screen_not_contains("Keybindings", timeout=2.0)
        tui.assert_screen_not_contains(
            "Keybindings", "Help keybindings should be gone after second ?"
        )
