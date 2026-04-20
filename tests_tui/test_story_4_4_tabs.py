"""Story 4-4 (continued): Multi-tab management.

AC: Ctrl+T creates new tab, number keys switch tabs, sidebar interaction.
Tab lifecycle: close tab, last-tab-close creates empty conversation.
"""

import pytest

from harness import RustainTUI
from keys import CTRL_T, TAB, SHIFT_TAB


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_4_4
@pytest.mark.story_5_0
class TestMultiTab:
    """Multi-tab creation and switching."""

    def test_new_tab_shows_tab_bar_with_two_tabs(self, tui: RustainTUI):
        """Ctrl+T creates a new tab — tab bar shows two tab entries."""
        tui.send_message("Say hello in tab 1")
        tui.wait_for_idle()

        tui.new_tab()
        # After creating a new tab, the tab bar should show at least two entries.
        # Tab 2 is fresh (title empty → rendered as "Tab 2").
        tui.wait_for_screen("Tab 2")
        tui.assert_screen_contains("Tab 2", "Tab bar should show the newly created Tab 2")

        # Send a message in the new tab to verify it works
        tui.send_message("Say hello in tab 2")
        tui.wait_for_idle()
        tui.assert_screen_contains("Say hello in tab 2", "Tab 2 message should appear in chat")

    def test_switch_tabs_changes_chat_pane_content(self, tui: RustainTUI):
        """Number keys 1-2 switch between tabs — chat pane content changes accordingly."""
        tui.send_message("Tab 1 message")
        tui.wait_for_screen("Tab 1 message")
        tui.wait_for_idle()

        tui.new_tab()
        tui.wait_for_screen("Tab 2")
        tui.wait(1.0)

        # Switch back to tab 1 — should show Tab 1's conversation
        tui.chat_mode()
        tui.switch_tab(1)
        tui.wait_for_screen("Tab 1 message")
        tui.assert_screen_contains(
            "Tab 1 message",
            "Switching to Tab 1 should show Tab 1's conversation content",
        )

        # Switch to tab 2 — should show empty/welcome state
        tui.switch_tab(2)
        tui.wait(0.5)
        # Tab 2 is fresh with no messages — empty state shows
        # (Tab 1 message should no longer be visible)
        tui.assert_screen_not_contains(
            "Tab 1 message",
            "Tab 1 message should not be visible when Tab 2 is active",
        )


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_4_4
@pytest.mark.story_5_0
class TestTabLifecycle:
    """Tab lifecycle: close tab, last-tab-close behavior."""

    def test_close_tab_reduces_tab_count(self, tui: RustainTUI):
        """Closing a tab removes it — tab bar no longer shows the closed tab entry."""
        tui.send_message("Tab 1 message for close test")
        tui.wait_for_idle()

        tui.new_tab()
        tui.wait_for_screen("Tab 2")

        # Close tab 2 (currently active) via command palette
        tui.chat_mode()
        tui.close_tab()
        tui.wait(1.0)

        # Tab 2 should be gone — back to a single tab (Tab 1's content)
        tui.assert_screen_not_contains(
            "Tab 2", "Tab 2 should be gone from the tab bar after closing"
        )

    def test_close_last_tab_is_rejected_with_flash(self, tui: RustainTUI):
        """Closing the last remaining tab is a no-op — status flashes 'Only one tab open'."""
        tui.send_message("Only tab message")
        tui.wait_for_idle()

        # Attempt to close the only tab via command palette
        tui.chat_mode()
        tui.close_tab()

        # The app prevents closing the last tab and flashes a message instead.
        # The conversation must remain visible and the TUI must stay alive.
        tui.wait_for_screen("Only one tab open", timeout=3.0)
        tui.assert_screen_contains(
            "Only one tab open",
            "Closing the last tab should show 'Only one tab open' flash",
        )
        tui.assert_responsive(timeout=3.0)
        tui.assert_screen_contains(
            "Only tab message",
            "Previous conversation must still be visible — last-tab close is a no-op",
        )
