"""Story 4-3a: Fork Conversations.

AC1: Fork trigger (f key) opens confirmation card
AC2: Fork creates new tab with truncated messages
AC3: Fork visual indicator
AC4: Fork independence
"""

import pytest

from harness import RustainTUI


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_4_3a
class TestForkTrigger:
    """AC1: f key in Chat focus opens the fork confirmation card."""

    def test_fork_cancel_is_noop(self, tui: RustainTUI):
        """Pressing f then n cancels — conversation unchanged."""
        tui.send_message("Say hello")
        tui.wait_for_idle()

        tui.chat_mode()
        tui.scroll_up(3)
        tui.fork_cancel()

        # No new tab should have been created.
        # We verify by checking that the conversation still has messages.
        tui.assert_log_not_contains(r"Forked conversation")


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_4_3a
class TestForkExecution:
    """AC2: Fork creates a new tab with messages up to the fork point."""

    def test_fork_creates_new_session(self, tui: RustainTUI):
        """After fork, a second session directory should exist."""
        tui.send_message("Say hello for fork test")
        tui.wait_for_idle()

        before_ids = set(tui.session_ids())

        tui.chat_mode()
        tui.scroll_up(3)
        tui.fork()

        after_ids = set(tui.session_ids())
        new_ids = after_ids - before_ids
        assert len(new_ids) >= 1, (
            f"Fork must create a new session. Before: {before_ids}, After: {after_ids}"
        )


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_4_3a
class TestForkFileIndependence:
    """AC4: Fork does NOT revert files (unlike rewind)."""

    def test_fork_preserves_tool_written_files(self, tui: RustainTUI):
        """Files created by tools persist after forking."""
        tui.send_message("Write 'fork file test' to fork_file.txt")
        # Tools auto-allowed via .claude/settings.json in temp workspace.
        tui.wait_for_idle()
        assert tui.file_exists("fork_file.txt")

        tui.chat_mode()
        tui.scroll_up(5)
        tui.fork()

        assert tui.file_exists("fork_file.txt"), (
            "Fork must NOT revert files — only rewind does"
        )
        tui.remove_file("fork_file.txt")
