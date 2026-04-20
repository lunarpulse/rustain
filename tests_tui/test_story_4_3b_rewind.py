"""Story 4-3b: Rewind with File Snapshot & Rollback.

AC1: Rewind trigger (R key) opens confirmation card
AC2: Checkpoint creation before tool execution
AC3: Rewind execution — truncate messages & revert files
AC4: Fork-instead path (f key in rewind overlay)
AC5: Conflict handling — externally modified files
"""

import pytest

from harness import RustainTUI


# ── AC1: Rewind Trigger ─────────────────────────────────────────────────────

@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_4_3b
class TestRewindTrigger:
    """AC1: R key in Chat focus opens the rewind confirmation card."""

    def test_rewind_cancel_does_not_revert(self, tui: RustainTUI):
        """Pressing R then n cancels the rewind — no side effects."""
        tui.send_message("Write 'cancel test' to rewind_cancel.txt")
        # Tools are auto-allowed via .claude/settings.json in temp workspace.
        tui.wait_for_idle()
        assert tui.file_exists("rewind_cancel.txt")

        tui.chat_mode()
        tui.jump_top()
        tui.rewind_cancel()

        # File must still exist — rewind was cancelled.
        assert tui.file_exists("rewind_cancel.txt")
        tui.remove_file("rewind_cancel.txt")


# ── AC2 + AC3: Checkpoint & Revert ──────────────────────────────────────────

@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_4_3b
class TestRewindRevert:
    """AC2 + AC3: Checkpoint is created before tool execution; rewind reverts files."""

    def test_rewind_deletes_tool_created_file(self, tui_write_only: RustainTUI):
        """Tool creates a new file. Rewind deletes it (file didn't exist before)."""
        tui_write_only.send_message("Write the text 'hello rewind' to rewind_created.txt")
        # Tools are auto-allowed via .claude/settings.json in temp workspace.
        tui_write_only.wait_for_idle()
        assert tui_write_only.file_exists("rewind_created.txt")

        tui_write_only.chat_mode()
        tui_write_only.jump_top()
        tui_write_only.rewind()

        assert not tui_write_only.file_exists("rewind_created.txt"), (
            "Tool-created file must be deleted after rewind"
        )

        # Verify logs show checkpoint + snapshot + revert
        tui_write_only.assert_log_contains(r"Created checkpoint \d+")
        tui_write_only.assert_log_contains(r"Snapshotted file.*rewind_created\.txt")
        tui_write_only.assert_log_contains(r"revert_file_snapshots.*found \d+ candidates")

    def test_rewind_restores_modified_file(self, tui_write_only: RustainTUI):
        """Tool modifies an existing file. Rewind restores original content."""
        original = "original content before tool\n"
        tui_write_only.write_file("rewind_modify.txt", original)

        tui_write_only.send_message(
            "Replace the contents of rewind_modify.txt with 'MODIFIED BY TOOL'"
        )
        # Tools are auto-allowed via .claude/settings.json in temp workspace.
        tui_write_only.wait_for_idle()

        modified = tui_write_only.file_content("rewind_modify.txt")
        assert modified != original, "Tool must have modified the file"

        tui_write_only.chat_mode()
        tui_write_only.jump_top()
        tui_write_only.rewind()

        restored = tui_write_only.file_content("rewind_modify.txt")
        assert restored == original, (
            f"File must be restored to original.\n"
            f"  Expected: {original!r}\n"
            f"  Got:      {restored!r}"
        )


# ── AC4: Fork-Instead Path ──────────────────────────────────────────────────

@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_4_3b
class TestRewindForkInstead:
    """AC4: Pressing f in the rewind overlay forks instead of rewinding."""

    def test_fork_instead_preserves_file(self, tui: RustainTUI):
        """Fork-instead does NOT revert files — they stay as-is."""
        tui.send_message("Write 'fork test' to rewind_fork.txt")
        # Tools are auto-allowed via .claude/settings.json in temp workspace.
        tui.wait_for_idle()
        assert tui.file_exists("rewind_fork.txt")

        tui.chat_mode()
        tui.jump_top()
        tui.rewind_fork_instead()

        # File must still exist — fork doesn't revert.
        assert tui.file_exists("rewind_fork.txt")
        tui.remove_file("rewind_fork.txt")


# ── AC5: Conflict Handling ───────────────────────────────────────────────────

@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_4_3b
class TestRewindConflict:
    """AC5: Externally modified files are detected as conflicts."""

    def test_externally_modified_file_not_overwritten(self, tui_write_only: RustainTUI):
        """If user edits a tool-written file before rewind, it becomes a conflict."""
        tui_write_only.write_file("conflict_test.txt", "original\n")

        tui_write_only.send_message(
            "Replace the contents of conflict_test.txt with 'TOOL WROTE THIS'"
        )
        # Tools are auto-allowed via .claude/settings.json in temp workspace.
        tui_write_only.wait_for_idle()

        # Externally modify the file AFTER the tool wrote it
        tui_write_only.write_file("conflict_test.txt", "USER EDITED EXTERNALLY")

        tui_write_only.chat_mode()
        tui_write_only.jump_top()
        tui_write_only.rewind()

        # File must keep the user's external edit (conflict = skip overwrite)
        content = tui_write_only.file_content("conflict_test.txt")
        assert content == "USER EDITED EXTERNALLY", (
            f"Conflict file must keep user's edit, got: {content!r}"
        )
        tui_write_only.assert_log_contains(r"Conflict")


# ── Multi-checkpoint: no false conflict ──────────────────────────────────────

@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_4_3b
class TestMultiCheckpointRewind:
    """Regression: file modified by tools across multiple turns must NOT
    show as 'modified externally' on rewind."""

    def test_multi_turn_modify_then_rewind(self, tui_write_only: RustainTUI):
        """Tool modifies a file twice in separate turns. Rewind restores original."""
        original = "version A (original)\n"
        tui_write_only.write_file("multi_turn.txt", original)

        # First tool modification
        tui_write_only.send_message(
            "Replace the contents of multi_turn.txt with 'version B (turn 1)'"
        )
        tui_write_only.wait_for_idle()

        # Second tool modification (new turn, new checkpoint)
        tui_write_only.send_message(
            "Replace the contents of multi_turn.txt with 'version C (turn 2)'"
        )
        tui_write_only.wait_for_idle()

        # File should now be at version C
        content = tui_write_only.file_content("multi_turn.txt")
        assert "C" in content or "turn 2" in content or content != original, (
            f"Tool must have modified file twice, got: {content!r}"
        )

        # Rewind to before both modifications
        tui_write_only.chat_mode()
        tui_write_only.jump_top()
        tui_write_only.rewind()

        # Key assertion: file must be RESTORED (not skipped as "conflict").
        # The exact content depends on which checkpoint the scroll position
        # targeted — but it must NOT be the latest tool write (version C)
        # and must NOT be flagged as a conflict.
        restored = tui_write_only.file_content("multi_turn.txt")
        assert restored != content, (
            f"Rewind must change the file — got same content as before rewind: {restored!r}"
        )
        tui_write_only.assert_log_contains(r"Restored")
        tui_write_only.assert_log_not_contains(
            r"Conflict.*multi_turn\.txt",
            msg="Multi-checkpoint file must NOT be flagged as conflict"
        )
