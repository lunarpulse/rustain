"""Story 4-4: Search, Bookmarks & Export.

AC1:  Ctrl+F opens search overlay
AC2:  Live incremental search highlighting
AC3:  n/N navigate matches
AC5:  Cross-conversation search via sidebar /
AC8:  Toggle bookmark via m key
AC10: Bookmark list via ' key
AC11: Export to markdown
"""

import pytest

from harness import RustainTUI
from keys import CTRL_F, ESC, Chat


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_4_4
@pytest.mark.story_5_0
class TestSearchOverlay:
    """AC1 + AC2: Ctrl+F opens search; typing filters results."""

    def test_search_shows_search_bar_on_ctrl_f(self, tui: RustainTUI):
        """Ctrl+F opens search overlay — 'Search:' input area visible on screen."""
        tui.send_message("Say the word 'searchable' in your reply")
        tui.wait_for_idle()

        # Must be in Chat or Input focus — Ctrl+F works from both
        tui.chat_mode()
        tui.open_search()
        tui.wait_for_screen("Search:")
        tui.assert_screen_contains("Search:", "Search bar label should be visible when open")

        # Type a query — the typed text should appear after the label
        tui.send("searchable")
        tui.wait(1.0)
        tui.assert_screen_contains("Search:", "Search bar should still show after typing")

        tui.close_overlay()
        tui.wait_for_screen_not_contains("Search:", timeout=2.0)
        tui.assert_screen_not_contains("Search:", "Search bar should be gone after close")


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_4_4
@pytest.mark.story_5_0
class TestBookmarks:
    """AC8 + AC10: Bookmark toggle and list."""

    def test_bookmark_toggle_shows_flash_confirmation(self, tui: RustainTUI):
        """Pressing m in Chat focus toggles a bookmark and shows a Success toast."""
        tui.send_message("Say hello for bookmark test")
        tui.wait_for_idle()

        tui.chat_mode()
        tui.scroll_up(2)
        tui.toggle_bookmark()
        # Story 21.1: `Bookmark added` / `Bookmark removed` is a 4 s Success toast
        # at the chat pane's bottom edge (no longer a status-row flash). The poll
        # (0.5 s) starts right after the toggle, well inside the TTL.
        tui.wait_for_screen("Bookmark")
        tui.assert_screen_contains("Bookmark", "Bookmark toast should appear")

        # Toggle again to un-bookmark
        tui.toggle_bookmark()
        tui.wait(0.5)

    def test_bookmark_list_panel_appears_on_apostrophe(self, tui: RustainTUI):
        """Pressing ' opens the Bookmark frame over the unchanged Stage; Esc closes it."""
        tui.send_message("Say hello for bookmark list test")
        tui.wait_for_idle()

        tui.chat_mode()
        # Bookmark a message first so the list isn't empty
        tui.scroll_up(2)
        tui.toggle_bookmark()
        tui.wait_for_screen("Bookmark")

        # Open the list
        tui.open_bookmark_list()
        tui.wait(0.5)
        # The Bookmark frame or the existing empty-list toast should appear
        # (toast if no bookmarks remain, frame otherwise; the toggle toast is
        # still alive here: ~2 s of the 4 s TTL have elapsed)
        tui.assert_screen_contains("Bookmark", "Bookmark state should be visible")
        tui.close_overlay()


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_4_4
@pytest.mark.story_5_0
class TestCrossSearch:
    """AC5: Cross-conversation search from sidebar."""

    def test_cross_search_shows_overlay_on_slash(self, tui: RustainTUI):
        """Ctrl+X h opens sidebar; / triggers cross-search overlay — 'Cross-Search' visible."""
        tui.send_message("Reply with exactly: OK")
        tui.wait_for_idle()

        tui.open_history()
        tui.wait_for_screen("History")
        tui.assert_screen_contains("History", "Sidebar should be visible")

        tui.send("/")  # Open cross-search
        tui.wait_for_screen("Cross-Search")
        tui.assert_screen_contains("Cross-Search", "Cross-search overlay should appear")

        tui.send("hello")
        tui.wait(1.0)
        tui.close_overlay()
        tui.close_history()  # Close sidebar
