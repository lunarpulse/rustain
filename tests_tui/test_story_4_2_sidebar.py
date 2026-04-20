"""Story 4-2: Conversation History Sidebar.

AC: Ctrl+H toggles sidebar, j/k navigate entries, Enter opens conversation.
Coverage gap tests for Epic 4 — part of Story 5-0 contract test upgrade.
"""

import pytest

from harness import RustainTUI
from keys import Sidebar, ENTER


@pytest.mark.requires_api
@pytest.mark.story_5_0
class TestSidebarToggle:
    """Sidebar toggle + visibility (4-2 AC)."""

    def test_sidebar_shows_history_panel_on_toggle(self, tui: RustainTUI):
        """Ctrl+H toggles sidebar visible — 'History' panel header appears on screen."""
        # Use a directive, non-tool-triggering prompt so the turn completes
        # quickly and deterministically without the model running bash/file tools.
        tui.send_message("Reply with exactly: OK")
        tui.wait_for_idle()

        tui.toggle_sidebar()
        tui.wait_for_screen("History")
        tui.assert_screen_contains("History", "Sidebar should show 'History' panel title")

        tui.toggle_sidebar()
        tui.wait_for_screen_not_contains("History", timeout=2.0)
        tui.assert_screen_not_contains(
            "History", "History panel should be hidden after second toggle"
        )

    def test_sidebar_navigate_with_j_k_stays_visible(self, tui: RustainTUI):
        """j/k navigate sidebar entries without crashing — sidebar remains visible."""
        tui.send_message("Reply with exactly: OK")
        tui.wait_for_idle()

        tui.toggle_sidebar()
        tui.wait_for_screen("History")
        tui.assert_screen_contains("History")

        # Navigate up and down — should not crash or close the sidebar
        tui.send(Sidebar.DOWN)  # j
        tui.wait(0.3)
        tui.send(Sidebar.UP)   # k
        tui.wait(0.3)

        # Sidebar should still be visible after navigation
        tui.assert_screen_contains(
            "History", "Sidebar should remain visible after j/k navigation"
        )

        tui.toggle_sidebar()

    def test_sidebar_open_conversation_with_enter(self, tui_in_project: RustainTUI):
        """Enter on a sidebar entry opens that conversation — sidebar closes on open."""
        tui_in_project.toggle_sidebar()
        tui_in_project.wait_for_screen("History")
        tui_in_project.assert_screen_contains("History")

        tui_in_project.send(Sidebar.DOWN)
        tui_in_project.wait(0.3)
        tui_in_project.send(Sidebar.OPEN)
        tui_in_project.wait(1.0)

        tui_in_project.assert_responsive(timeout=3.0)
        tui_in_project.assert_screen_not_contains(
            "History", "Sidebar should close after opening a conversation"
        )
        tui_in_project.assert_screen_contains(
            "Ready", "TUI should show main chat view after sidebar conversation open"
        )
        tui_in_project.toggle_sidebar()
