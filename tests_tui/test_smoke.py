"""Smoke tests — verify the TUI starts, renders, and shuts down cleanly.

These do NOT require an API key and run in every CI pipeline.
"""

import subprocess
from pathlib import Path

import pytest

from harness import RustainTUI, PROJECT_ROOT, BINARY


class TestBinaryExists:
    """Verify cargo build produces a working binary."""

    def test_binary_built(self, build_binary):
        assert BINARY.exists()

    def test_binary_runs_help(self):
        """--help should print usage and exit 0."""
        result = subprocess.run(
            [str(BINARY), "--help"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        assert result.returncode == 0
        assert "rustain" in result.stdout.lower() or "usage" in result.stdout.lower()

    def test_binary_runs_version(self):
        """--version should print version and exit 0."""
        result = subprocess.run(
            [str(BINARY), "--version"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        assert result.returncode == 0


class TestTUILifecycle:
    """Verify the TUI can start and stop without crashing."""

    @pytest.mark.requires_api
    def test_start_fresh_and_quit(self, tui: RustainTUI):
        """Start with --new, wait for render, then quit cleanly."""
        # tui fixture handles start/stop — reaching here = no crash
        assert tui.child.isalive()

    @pytest.mark.requires_api
    def test_log_file_created(self, tui: RustainTUI):
        """Starting the TUI creates a log file."""
        tui.assert_log_contains(r"Starting rustain")

    @pytest.mark.requires_api
    def test_workspace_created(self, tui: RustainTUI):
        """The workspace directory exists (temp dir for test isolation)."""
        assert tui.wp.exists()
