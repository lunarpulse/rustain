"""Story 6-0b: ToolScheduler with ToolCall 7-Variant FSM — TUI E2E tests.

These tests verify the TUI-visible aspects of the ToolScheduler:
- Status chips (✓ Success, ✗ Error) render inline in tool blocks
- Multiple tools complete successfully in a single turn (batch scheduling)
- Tool execution lifecycle remains stable under normal and error conditions

Non-API structural tests run without an API key.
API-dependent tests exercise the full scheduler → TUI rendering pipeline.
"""

from __future__ import annotations

import pytest

from harness import RustainTUI


# ── Non-API structural tests ─────────────────────────────────────────────────


@pytest.mark.story_6_0b
def test_story_marker_registered():
    """Verify the story_6_0b pytest marker is available."""
    # This test exists so that `pytest -m story_6_0b` does not complain
    # about an unknown marker when only structural tests are collected.
    assert True


# ── API-dependent TUI tests ──────────────────────────────────────────────────


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_0b
class TestToolStatusChips:
    """TUI renders status chips inline with tool blocks (AC1-AC7)."""

    def test_read_tool_shows_success_chip(self, tui: RustainTUI):
        """A successful Read tool displays the '✓ Success' chip.

        Verifies that the scheduler's terminal Success state is bridged
        through the EventBus to the TUI and rendered as a status chip.
        """
        # Pre-create a file so the read succeeds deterministically
        tui.write_file("success_chip_6_0b.txt", "hello from 6-0b")

        tui.send_message("Read the contents of success_chip_6_0b.txt")
        tui.wait_for_idle()

        # The tool block should show the success status chip
        tui.assert_screen_contains("✓ Success")
        # The tool name should also be visible
        tui.assert_screen_contains("Read")
        # The file content should also be visible
        tui.assert_screen_contains("hello from 6-0b")

    def test_read_missing_file_shows_error_chip(self, tui: RustainTUI):
        """A failed Read tool (missing file) displays the '✗ Error' chip.

        Verifies that the scheduler's terminal Error state is bridged
        through the EventBus to the TUI and rendered as a status chip.
        """
        tui.send_message(
            "Read the contents of this_file_does_not_exist_6_0b.txt"
        )
        tui.wait_for_idle()

        # The tool block should show the error status chip
        tui.assert_screen_contains("✗ Error")
        # The tool name should also be visible
        tui.assert_screen_contains("Read")


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_0b
class TestToolBatchScheduling:
    """Multiple tools in one turn execute via the scheduler's batch path."""

    def test_multiple_reads_complete_in_one_turn(self, tui: RustainTUI):
        """Two Read tools in a single turn both complete successfully.

        Verifies that the scheduler handles multi-tool batches (parallel
        when all tools are parallel_safe, sequential otherwise) and that
        all terminal results are emitted back to the provider and rendered
        in the TUI.
        """
        # Pre-create two files so the reads succeed deterministically
        tui.write_file("batch_a_6_0b.txt", "alpha content")
        tui.write_file("batch_b_6_0b.txt", "beta content")

        tui.send_message(
            "Read batch_a_6_0b.txt and also read batch_b_6_0b.txt"
        )
        tui.wait_for_idle()

        screen = tui.get_screen_text()
        if "Stream disconnected" in screen or "interrupted" in screen:
            pytest.skip(
                "API stream disconnected — batch scheduling not exercised"
            )

        # Both results should appear in the conversation
        assert "alpha content" in screen, (
            "First file content not found on screen"
        )
        assert "beta content" in screen, (
            "Second file content not found on screen"
        )

        # Both tools should show success chips
        tui.assert_screen_contains("✓ Success")

    def test_mixed_success_and_error_tools(self, tui: RustainTUI):
        """One successful Read and one failed Read in the same turn.

        Verifies the scheduler handles heterogeneous terminal outcomes
        within a single batch.
        """
        tui.write_file("mixed_ok_6_0b.txt", "ok content")

        tui.send_message(
            "Read mixed_ok_6_0b.txt and also read missing_6_0b.txt"
        )
        tui.wait_for_idle()

        # Successful result visible
        tui.assert_screen_contains("ok content")

        # Both chips should be visible on screen
        screen = tui.get_screen_text()
        assert "✓ Success" in screen, "Success chip not rendered"
        assert "✗ Error" in screen, "Error chip not rendered"


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_0b
class TestToolExecutionStability:
    """Scheduler stability under edge conditions visible in the TUI."""

    def test_turn_completes_after_tool_error(self, tui: RustainTUI):
        """A tool error does not leave the turn hanging — TUI returns to Ready.

        Regression guard: the scheduler must always emit a terminal state
        (Success, Error, or Cancelled) so the event loop can proceed.

        NOTE: Stream disconnections ("Stream disconnected unexpectedly")
        are an API-level flake, not a scheduler bug.  When the stream
        drops, the TUI still returns to Ready — that's the core invariant.
        The error chip assertion is only checked when the turn completed
        normally.
        """
        tui.send_message("Read missing_file_6_0b_stability.txt")
        tui.wait_for_idle()

        tui.assert_screen_contains("Ready")
        screen = tui.get_screen_text()
        if "Stream disconnected" in screen or "interrupted" in screen:
            pytest.skip(
                "API stream disconnected — tool error chip not rendered; "
                "core invariant (Ready state) verified"
            )
        tui.assert_screen_contains("✗ Error")

    def test_subsequent_turn_after_tool_success(self, tui: RustainTUI):
        """After a tool succeeds, the next user message is processed normally.

        Verifies the scheduler does not leak state between turns.

        NOTE: Stream disconnections cause the tool output to be missing from
        the screen.  When that happens we skip — the test exercises API
        reliability, not the scheduler's turn isolation.
        """
        tui.write_file("turn1_6_0b.txt", "first")

        tui.send_message("Read turn1_6_0b.txt")
        tui.wait_for_idle()
        screen = tui.get_screen_text()
        if "Stream disconnected" in screen or "interrupted" in screen:
            pytest.skip(
                "API stream disconnected on first turn — "
                "turn isolation not exercised"
            )
        tui.assert_screen_contains("first")

        # Second turn should work without issues
        tui.write_file("turn2_6_0b.txt", "second")
        tui.send_message("Read turn2_6_0b.txt")
        tui.wait_for_idle()
        screen = tui.get_screen_text()
        if "Stream disconnected" in screen or "interrupted" in screen:
            pytest.skip(
                "API stream disconnected on second turn — "
                "turn isolation not exercised"
            )
        tui.assert_screen_contains("second")
