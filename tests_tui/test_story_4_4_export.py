"""Story 4-4: Export to Markdown.

AC11: /export [filename] writes conversation to a markdown file in the workspace.
Coverage gap tests for Epic 4 — part of Story 5-0 contract test upgrade.
"""

import pytest

from harness import RustainTUI
from keys import ENTER


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_4_4
@pytest.mark.story_5_0
class TestExportToMarkdown:
    """AC11: /export command creates a markdown file with conversation content."""

    def test_export_creates_markdown_file_in_workspace(self, tui: RustainTUI):
        """Typing /export <name> creates the file under .rustain/exports/."""
        export_filename = "test_export.md"

        # Use a neutral, non-tool-triggering prompt so the conversation has
        # content but the turn completes quickly without waiting on tools.
        tui.send_message("Reply with exactly: OK")
        tui.wait_for_idle()

        # Trigger export via slash command — /export <filename>
        tui.send_message(f"/export {export_filename}")

        # /export writes to {workspace}/.rustain/exports/<filename>, not workspace root.
        # Poll for the file instead of a fixed wait — the command runs through
        # spawn_blocking and may trail by more than 3s under load.
        assert tui.wait_for_export_file(export_filename, timeout=10.0), (
            f"Export file '{export_filename}' should exist under .rustain/exports/ after /export"
        )

    def test_export_file_contains_conversation_content(self, tui: RustainTUI):
        """The exported markdown file contains the conversation messages."""
        export_filename = "test_export_content.md"
        unique_marker = "unique-export-test-string-42"

        # Send a distinctive message so we can verify it appears in the export.
        # Pinning the exact reply keeps the turn fast and deterministic.
        tui.send_message(f"Reply with exactly: {unique_marker}")
        tui.wait_for_idle()

        tui.send_message(f"/export {export_filename}")

        assert tui.wait_for_export_file(export_filename, timeout=10.0), (
            f"Export file should exist under .rustain/exports/"
        )

        content = tui.export_file_content(export_filename)
        assert len(content) > 0, "Exported file should not be empty"
        assert unique_marker in content, (
            f"Exported markdown should contain the unique marker '{unique_marker}'. "
            f"First 500 chars: {content[:500]}"
        )
