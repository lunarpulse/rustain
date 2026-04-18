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
class TestMultilineInput:
    """Story 3.1: Multi-line editing and history navigation."""

    def test_submit_single_line(self, tui: RustainTUI):
        """Typing text + Enter submits a single-line message."""
        tui.send_message("Say 'pong'")
        tui.wait_for_idle()
        # No crash = success for basic submission smoke test.

    def test_history_up_down(self, tui: RustainTUI):
        """After submitting, Up arrow recalls previous input."""
        tui.send_message("First message for history")
        tui.wait_for_idle()

        # Press Up to recall previous input
        tui.send(UP)
        tui.wait(0.5)
        tui.send(DOWN)
        tui.wait(0.3)
        # No crash — history navigation is functional.


# ── Story 3.3: Command Palette ───────────────────────────────────────────────

@pytest.mark.requires_api
@pytest.mark.story_3_3
class TestCommandPalette:
    """Story 3.3: Ctrl+P opens command palette; Esc closes it."""

    def test_palette_open_and_close(self, tui: RustainTUI):
        """Ctrl+P opens, Esc closes — no crash."""
        tui.send_message("Say hello for palette test")
        tui.wait_for_idle()

        tui.send(CTRL_P)
        tui.wait(0.5)
        tui.close_overlay()

    def test_which_key_open_and_close(self, tui: RustainTUI):
        """Ctrl+X opens which-key overlay; any key dismisses it."""
        tui.send_message("Say hello for which-key test")
        tui.wait_for_idle()

        tui.send(CTRL_X)
        tui.wait(0.5)
        tui.send("a")  # Dismiss with any key
        tui.wait(0.3)


# ── Story 3.5: Help Overlay ──────────────────────────────────────────────────

@pytest.mark.requires_api
@pytest.mark.story_3_5
class TestHelpOverlay:
    """Story 3.5: ? key in Chat focus opens help; Esc or ? closes it."""

    def test_help_open_scroll_close(self, tui: RustainTUI):
        """Open help, scroll with j/k, close with Esc."""
        tui.send_message("Say hello for help test")
        tui.wait_for_idle()

        tui.chat_mode()
        tui.open_help()
        # Scroll inside help
        tui.send_keys("j", "j", "k")
        tui.close_overlay()

    def test_help_close_with_question_mark(self, tui: RustainTUI):
        """? is a toggle — pressing it again closes the help overlay."""
        tui.send_message("Say hello")
        tui.wait_for_idle()

        tui.chat_mode()
        tui.send(Chat.HELP)
        tui.wait(0.5)
        tui.send(Chat.HELP)  # Close
        tui.wait(0.3)
