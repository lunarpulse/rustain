"""Pytest configuration and shared fixtures for rustain TUI E2E tests."""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

import pytest

# Ensure tests_tui/ is on the import path
sys.path.insert(0, str(Path(__file__).parent))

from harness import RustainTUI, PROJECT_ROOT, BINARY


# ── Guards ───────────────────────────────────────────────────────────────────

def pytest_configure(config):
    """Register custom markers."""
    # Markers are declared in pyproject.toml; this is a fallback.
    pass


def pytest_collection_modifyitems(config, items):
    """Auto-skip requires_api tests when ANTHROPIC_API_KEY is not set."""
    has_key = bool(os.environ.get("ANTHROPIC_API_KEY") or os.environ.get("ANTHROPIC_AUTH_TOKEN"))

    # Also check .env file
    env_file = PROJECT_ROOT / ".env"
    if not has_key and env_file.exists():
        for line in env_file.read_text().splitlines():
            line = line.strip().removeprefix("export ").strip()
            if line.startswith("ANTHROPIC_API_KEY=") or line.startswith("ANTHROPIC_AUTH_TOKEN="):
                val = line.split("=", 1)[1].strip()
                if val and val != '""' and val != "''":
                    has_key = True
                    break

    if not has_key:
        skip_api = pytest.mark.skip(reason="No API key — set ANTHROPIC_API_KEY or add to .env")
        for item in items:
            if "requires_api" in item.keywords:
                item.add_marker(skip_api)


# ── Fixtures ─────────────────────────────────────────────────────────────────

@pytest.fixture(scope="session", autouse=True)
def build_binary():
    """Build the rustain binary once per test session."""
    result = subprocess.run(
        ["cargo", "build"],
        cwd=PROJECT_ROOT,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        pytest.fail(f"cargo build failed:\n{result.stderr}")
    if not BINARY.exists():
        pytest.fail(f"Binary not at {BINARY}")


@pytest.fixture
def tui(build_binary):
    """Provide a fresh RustainTUI instance (auto-cleanup).

    The TUI starts with --new (fresh conversation) and uses a
    temporary workspace so tests are isolated from each other.

    Requires @pytest.mark.requires_api for tests that send messages.
    """
    harness = RustainTUI(fresh=True, build=False)
    harness.start()
    yield harness
    harness.stop()


@pytest.fixture
def tui_in_project(build_binary):
    """Provide a RustainTUI running inside the actual project workspace.

    Use this when tests need the real .env, CLAUDE.md, or existing
    session data.  Mutations (file writes, session creation) happen
    in the REAL workspace — use sparingly and clean up after.
    """
    harness = RustainTUI(fresh=True, build=False, workspace=PROJECT_ROOT)
    harness.start()
    yield harness
    harness.stop()


@pytest.fixture(autouse=True)
def _reset_tui_state(request):
    """After each test, send Esc to close any open overlays and return to
    a known state.  Only applies to tests that used a ``tui`` or
    ``tui_in_project`` fixture.
    """
    yield
    tui_fixtures = {"tui", "tui_in_project"}
    if not tui_fixtures.intersection(request.fixturenames):
        return
    tui_instance = request.getfixturevalue(
        next(iter(tui_fixtures.intersection(request.fixturenames)))
    )
    if tui_instance is None:
        return
    try:
        from keys import ESC
        for _ in range(3):
            tui_instance.send(ESC)
        tui_instance.wait_for_screen("Ready", timeout=2.0)
    except Exception:
        pass


@pytest.fixture
def workspace(tmp_path):
    """Provide a temporary workspace directory (no TUI, for unit-style helpers)."""
    return tmp_path
