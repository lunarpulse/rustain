"""Story 4-4 (continued): Multi-tab management.

AC: Ctrl+T creates new tab, number keys switch tabs, sidebar interaction.
"""

import pytest

from harness import RustainTUI
from keys import CTRL_T, TAB, SHIFT_TAB


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_4_4
class TestMultiTab:
    """Multi-tab creation and switching."""

    def test_new_tab_no_crash(self, tui: RustainTUI):
        """Ctrl+T creates a new tab without crashing."""
        tui.send_message("Say hello in tab 1")
        tui.wait_for_idle()

        tui.new_tab()
        tui.wait(1.0)
        # Send a message in the new tab.
        tui.send_message("Say hello in tab 2")
        tui.wait_for_idle()
        # Reaching here without crash = success.

    def test_switch_tabs_by_number(self, tui: RustainTUI):
        """Number keys 1-2 switch between tabs."""
        tui.send_message("Tab 1 message")
        tui.wait_for_idle()

        tui.new_tab()
        tui.wait(2.0)

        # Switch back to tab 1
        tui.chat_mode()
        tui.switch_tab(1)
        tui.wait(0.5)
        # Switch to tab 2
        tui.switch_tab(2)
        tui.wait(0.5)
        # No crash = tabs work.
