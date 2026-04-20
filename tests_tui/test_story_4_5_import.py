"""Story 4-5: History Import CLI.

Tests the `rustain migrate --from claude-code` subcommand.
These are subprocess-level tests — no full TUI required.
Coverage gap tests for Epic 4 — part of Story 5-0 contract test upgrade.
"""

from __future__ import annotations

import json
import subprocess
import tempfile
import uuid
from pathlib import Path

import pytest

from harness import BINARY, PROJECT_ROOT


# ── Helpers ─────────────────────────────────────────────────────────────────

def _make_claude_code_project(root: Path) -> Path:
    """Create a minimal fake Claude Code projects directory.

    Layout:  root/{workspace_hash}/{session_uuid}.jsonl
    Returns the root path.
    """
    import hashlib
    workspace_hash = hashlib.sha256(b"test-workspace").hexdigest()[:16]
    session_uuid = str(uuid.uuid4())

    session_dir = root / workspace_hash
    session_dir.mkdir(parents=True)

    lines = []
    lines.append(json.dumps({
        "type": "user",
        "uuid": str(uuid.uuid4()),
        "timestamp": "2026-04-01T10:00:00Z",
        "message": {
            "role": "user",
            "content": "Hello from import test",
        },
    }))
    lines.append(json.dumps({
        "type": "assistant",
        "uuid": str(uuid.uuid4()),
        "timestamp": "2026-04-01T10:00:05Z",
        "message": {
            "role": "assistant",
            "content": [{"type": "text", "text": "Hello! This is a migrated response."}],
        },
    }))

    (session_dir / f"{session_uuid}.jsonl").write_text("\n".join(lines) + "\n")
    return root


# ── Tests ────────────────────────────────────────────────────────────────────

class TestMigrateCliSubcommand:
    """rustain migrate --from claude-code runs and imports sessions."""

    def test_migrate_dry_run_discovers_session(self):
        """--dry-run mode discovers the fake session without writing any files."""
        with tempfile.TemporaryDirectory(prefix="rustain_import_src_") as src_dir, \
             tempfile.TemporaryDirectory(prefix="rustain_workspace_") as ws_dir:

            _make_claude_code_project(Path(src_dir))

            result = subprocess.run(
                [str(BINARY), "migrate", "--from", "claude-code",
                 "--path", src_dir, "--dry-run"],
                cwd=ws_dir,
                capture_output=True,
                text=True,
                timeout=30,
            )

            assert result.returncode == 0, (
                f"migrate --dry-run should succeed\nstdout: {result.stdout}\nstderr: {result.stderr}"
            )
            output = result.stdout + result.stderr
            assert any(
                word in output.lower()
                for word in ("found", "session", "conversation", "dry run", "import")
            ), f"Dry run output should describe discovered sessions:\n{output}"
            assert "dry run" in output.lower(), (
                f"Dry run output should explicitly mention 'dry run':\n{output}"
            )

            # No session meta files should be written to disk in dry-run mode
            sessions_dir = Path(ws_dir) / ".claude" / "sessions"
            session_meta_files = (
                [f for f in sessions_dir.iterdir() if f.name.endswith(".meta.json")]
                if sessions_dir.exists()
                else []
            )
            assert len(session_meta_files) == 0, (
                f"Dry run must not write session files to disk, found: {session_meta_files}"
            )

    def test_migrate_yes_imports_session_and_creates_files(self):
        """--yes flag auto-confirms import — session files appear in workspace."""
        with tempfile.TemporaryDirectory(prefix="rustain_import_src_") as src_dir, \
             tempfile.TemporaryDirectory(prefix="rustain_workspace_") as ws_dir:

            _make_claude_code_project(Path(src_dir))

            result = subprocess.run(
                [str(BINARY), "migrate", "--from", "claude-code",
                 "--path", src_dir, "--yes"],
                cwd=ws_dir,
                capture_output=True,
                text=True,
                timeout=30,
            )

            assert result.returncode == 0, (
                f"migrate --yes should succeed\nstdout: {result.stdout}\nstderr: {result.stderr}"
            )
            output = result.stdout + result.stderr
            assert "Imported" in output, (
                f"Output should confirm sessions were imported:\n{output}"
            )
            assert "1" in output, "Should report 1 imported conversation"

            # Sessions must exist in the workspace after import.
            # Storage format: flat files — {id}.meta.json + {id}.session.json per session.
            sessions_dir = Path(ws_dir) / ".claude" / "sessions"
            assert sessions_dir.exists(), "sessions/ directory should be created after import"
            session_files = [f for f in sessions_dir.iterdir() if f.name.endswith(".meta.json")]
            assert len(session_files) == 1, (
                f"Exactly 1 .meta.json session file should exist after import, "
                f"found {len(session_files)}: {[f.name for f in session_files]}"
            )

    def test_migrate_unknown_source_exits_nonzero(self):
        """Unknown --from value exits with non-zero and helpful error message."""
        with tempfile.TemporaryDirectory(prefix="rustain_workspace_") as ws_dir:
            result = subprocess.run(
                [str(BINARY), "migrate", "--from", "nonexistent-tool"],
                cwd=ws_dir,
                capture_output=True,
                text=True,
                timeout=15,
            )

            assert result.returncode != 0, "Unknown source should exit with non-zero"
            output = result.stdout + result.stderr
            assert "Unsupported" in output or "supported" in output.lower(), (
                f"Error should mention supported sources:\n{output}"
            )

    def test_migrate_idempotent_second_import_skips_duplicates(self):
        """Running migrate twice imports 0 new sessions on the second run."""
        with tempfile.TemporaryDirectory(prefix="rustain_import_src_") as src_dir, \
             tempfile.TemporaryDirectory(prefix="rustain_workspace_") as ws_dir:

            _make_claude_code_project(Path(src_dir))

            common_args = [
                str(BINARY), "migrate", "--from", "claude-code",
                "--path", src_dir, "--yes",
            ]

            # First import
            r1 = subprocess.run(
                common_args, cwd=ws_dir, capture_output=True, text=True, timeout=30
            )
            assert r1.returncode == 0

            sessions_dir = Path(ws_dir) / ".claude" / "sessions"
            files_after_first = set(
                f.name for f in sessions_dir.iterdir() if f.name.endswith(".meta.json")
            )

            r2 = subprocess.run(
                common_args, cwd=ws_dir, capture_output=True, text=True, timeout=30
            )
            assert r2.returncode == 0, (
                f"Second migrate should succeed:\n{r2.stdout}\n{r2.stderr}"
            )
            output2 = r2.stdout + r2.stderr
            assert any(
                word in output2
                for word in ("0 new", "already imported", "Skipped", "skipped")
            ), f"Second import should report skipped/duplicate:\n{output2}"

            files_after_second = set(
                f.name for f in sessions_dir.iterdir() if f.name.endswith(".meta.json")
            )
            assert files_after_first == files_after_second, (
                f"Second import should not create new session files. "
                f"Before: {files_after_first}, After: {files_after_second}"
            )
