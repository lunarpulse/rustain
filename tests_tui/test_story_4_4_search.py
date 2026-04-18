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
class TestSearchOverlay:
    """AC1 + AC2: Ctrl+F opens search; typing filters results."""

    def test_search_open_and_close(self, tui: RustainTUI):
        """Ctrl+F opens search overlay; Esc closes it."""
        tui.send_message("Say the word 'searchable' in your reply")
        tui.wait_for_idle()

        # Must be in Chat or Input focus — Ctrl+F works from both
        # (Input requires empty buffer per AC1 guard).
        tui.chat_mode()
        tui.open_search()
        # Type a query
        tui.send("searchable")
        tui.wait(1.0)
        tui.close_overlay()

        # No crash, no hang — basic smoke test.


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_4_4
class TestBookmarks:
    """AC8 + AC10: Bookmark toggle and list."""

    def test_bookmark_toggle_no_crash(self, tui: RustainTUI):
        """Pressing m in Chat focus toggles a bookmark without crashing."""
        tui.send_message("Say hello for bookmark test")
        tui.wait_for_idle()

        tui.chat_mode()
        tui.scroll_up(2)
        tui.toggle_bookmark()
        # Toggle again (un-bookmark)
        tui.toggle_bookmark()

    def test_bookmark_list_opens_and_closes(self, tui: RustainTUI):
        """Pressing ' opens the bookmark list panel; Esc closes it."""
        tui.send_message("Say hello for bookmark list test")
        tui.wait_for_idle()

        tui.chat_mode()
        # Bookmark a message first
        tui.scroll_up(2)
        tui.toggle_bookmark()
        # Open the list
        tui.open_bookmark_list()
        tui.wait(0.5)
        tui.close_overlay()


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_4_4
class TestCrossSearch:
    """AC5: Cross-conversation search from sidebar."""

    def test_sidebar_cross_search_opens(self, tui: RustainTUI):
        """Ctrl+H opens sidebar; / triggers cross-search overlay."""
        tui.send_message("Say hello for cross search")
        tui.wait_for_idle()

        tui.toggle_sidebar()
        tui.send("/")  # Open cross-search
        tui.wait(0.5)
        tui.send("hello")
        tui.wait(1.0)
        tui.close_overlay()
        tui.toggle_sidebar()  # Close sidebar
