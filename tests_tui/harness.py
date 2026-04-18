"""RustainTUI — pexpect harness for E2E TUI testing.

Usage::

    from harness import RustainTUI

    with RustainTUI() as tui:
        tui.send_message("Write 'hello' to test.txt")
        tui.approve_permission()
        tui.wait_for_idle()
        assert tui.file_exists("test.txt")

        tui.chat_mode()
        tui.scroll_up(5)
        tui.rewind()
        assert not tui.file_exists("test.txt")
"""

from __future__ import annotations

import os
import re
import time
import shutil
import tempfile
from contextlib import contextmanager
from dataclasses import dataclass, field
from pathlib import Path
from typing import Iterator

import pexpect

from keys import (
    ESC, ENTER, CTRL_C, CTRL_F, CTRL_H, CTRL_T,
    Chat, Confirm, Permission, Sidebar,
)

# ── Configuration ────────────────────────────────────────────────────────────

# Project root — one level up from tests_tui/
PROJECT_ROOT = Path(__file__).resolve().parent.parent
BINARY = PROJECT_ROOT / "target" / "debug" / "rustain"
ENV_FILE = PROJECT_ROOT / ".env"
LOG_DIR = Path.home() / ".rustain"

# Terminal dimensions for the spawned PTY
TERM_ROWS = 30
TERM_COLS = 100

# Timing defaults (seconds)
STARTUP_WAIT = 3.0
KEY_DELAY = 0.2
PERMISSION_WAIT = 10.0
TOOL_EXEC_WAIT = 15.0
TURN_COMPLETE_WAIT = 15.0
REWIND_SETTLE = 3.0


# ── Helpers ──────────────────────────────────────────────────────────────────

def _load_env() -> dict[str, str]:
    """Merge process env with project .env file."""
    env = os.environ.copy()
    if ENV_FILE.exists():
        for line in ENV_FILE.read_text().splitlines():
            line = line.strip()
            if line.startswith("export "):
                line = line[7:]
            if "=" in line and not line.startswith("#"):
                key, _, val = line.partition("=")
                env[key.strip()] = val.strip()
    return env


def _build_binary() -> Path:
    """Ensure the debug binary is up-to-date. Returns binary path."""
    import subprocess

    result = subprocess.run(
        ["cargo", "build"],
        cwd=PROJECT_ROOT,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(f"cargo build failed:\n{result.stderr}")
    assert BINARY.exists(), f"Binary not found at {BINARY}"
    return BINARY


def _today_log() -> Path | None:
    """Return today's log file path, or None."""
    from datetime import date

    log_file = LOG_DIR / f"rustain.log.{date.today()}"
    return log_file if log_file.exists() else None


# ── Main Harness ─────────────────────────────────────────────────────────────

@dataclass
class RustainTUI:
    """Spawns a rustain TUI process and provides methods to drive it.

    Use as a context manager for automatic cleanup::

        with RustainTUI(fresh=True) as tui:
            tui.send_message("hello")
    """

    fresh: bool = True
    """Start with --new flag (fresh conversation, no session restore)."""

    build: bool = True
    """Run cargo build before spawning."""

    workspace: Path | None = None
    """Override workspace directory. Defaults to a temporary directory."""

    timeout: float = 60.0
    """pexpect timeout for expect() calls."""

    _child: pexpect.spawn | None = field(default=None, init=False, repr=False)
    _tmpdir: tempfile.TemporaryDirectory | None = field(
        default=None, init=False, repr=False
    )
    _workspace_path: Path | None = field(default=None, init=False, repr=False)
    _log_line_offset: int = field(default=0, init=False, repr=False)

    # ── Lifecycle ────────────────────────────────────────────────────────

    def start(self) -> "RustainTUI":
        """Build (if requested) and spawn the TUI process."""
        if self.build:
            _build_binary()

        # Use a temp workspace so tests don't pollute the real project.
        if self.workspace is None:
            self._tmpdir = tempfile.TemporaryDirectory(prefix="rustain_test_")
            self._workspace_path = Path(self._tmpdir.name)
            # Copy .env so the API key is available
            if ENV_FILE.exists():
                shutil.copy2(ENV_FILE, self._workspace_path / ".env")
            # Pre-create .claude/settings.json with AlwaysAllow for
            # common tools so tests don't hang on permission prompts.
            settings_dir = self._workspace_path / ".claude"
            settings_dir.mkdir(parents=True, exist_ok=True)
            (settings_dir / "settings.json").write_text(
                '{"permissions":{"allow":["Read","Write","Edit","Bash","Glob","Grep"]}}\n'
            )
        else:
            self._workspace_path = self.workspace

        # Record current log length to read only NEW entries later
        log = _today_log()
        if log:
            self._log_line_offset = sum(1 for _ in log.open())

        env = _load_env()
        args = [str(BINARY)]
        if self.fresh:
            args.append("--new")

        self._child = pexpect.spawn(
            args[0],
            args=args[1:],
            cwd=str(self._workspace_path),
            env=env,
            timeout=self.timeout,
            encoding="utf-8",
            dimensions=(TERM_ROWS, TERM_COLS),
        )

        time.sleep(STARTUP_WAIT)
        return self

    def stop(self) -> None:
        """Gracefully shut down the TUI."""
        if self._child and self._child.isalive():
            self._child.sendcontrol("c")
            time.sleep(0.5)
            try:
                self._child.close(force=True)
            except Exception:
                pass
        if self._tmpdir:
            self._tmpdir.cleanup()
            self._tmpdir = None

    def __enter__(self) -> "RustainTUI":
        return self.start()

    def __exit__(self, *exc) -> None:
        self.stop()

    # ── Low-level I/O ────────────────────────────────────────────────────

    @property
    def child(self) -> pexpect.spawn:
        assert self._child is not None, "TUI not started — call .start() first"
        return self._child

    @property
    def wp(self) -> Path:
        """Workspace path."""
        assert self._workspace_path is not None
        return self._workspace_path

    def send(self, data: str) -> None:
        """Send raw bytes/string to the TUI."""
        self.child.send(data)

    def sendline(self, line: str) -> None:
        """Send a line (appends \\r)."""
        self.child.sendline(line)

    def send_keys(self, *keys: str, delay: float = KEY_DELAY) -> None:
        """Send a sequence of keystrokes with delay between each."""
        for k in keys:
            self.child.send(k)
            time.sleep(delay)

    def wait(self, seconds: float) -> None:
        """Sleep for a fixed duration."""
        time.sleep(seconds)

    # ── High-level Actions ───────────────────────────────────────────────

    def send_message(self, text: str, char_delay: float = 0.01) -> None:
        """Type a message into the input box and submit it.

        Characters are sent individually with a small delay because
        raw-mode TUIs drop burst input from ``sendline()``.
        Assumes focus is on the Input box (default after startup).
        """
        for c in text:
            self.child.send(c)
            time.sleep(char_delay)
        self.child.send(ENTER)

    def chat_mode(self) -> None:
        """Switch to Chat focus (press Escape)."""
        self.send(ESC)
        time.sleep(0.5)

    def input_mode(self) -> None:
        """Switch to Input focus (press 'i' from Chat)."""
        self.send(Chat.FOCUS_INPUT)
        time.sleep(0.3)

    def scroll_up(self, n: int = 1) -> None:
        """Scroll up n lines in Chat focus."""
        self.send_keys(*([Chat.SCROLL_UP] * n))

    def scroll_down(self, n: int = 1) -> None:
        """Scroll down n lines in Chat focus."""
        self.send_keys(*([Chat.SCROLL_DOWN] * n))

    def jump_top(self) -> None:
        """Jump to top of conversation."""
        self.send(Chat.JUMP_TOP)
        time.sleep(0.3)

    def jump_bottom(self) -> None:
        """Jump to bottom of conversation."""
        self.send(Chat.JUMP_BOTTOM)
        time.sleep(0.3)

    def approve_permission(self) -> None:
        """Send 'y' to approve a tool permission prompt.

        Note: when using the ``tui`` fixture, tools are pre-allowed via
        ``.claude/settings.json`` in the temp workspace.  Call this only
        for tools NOT in the allow list, or when testing permission UI.
        """
        self.send(Permission.ALLOW)
        time.sleep(1.0)

    def always_allow_permission(
        self, wait_before: float = PERMISSION_WAIT
    ) -> None:
        """Press 'a' to always-allow the tool permission."""
        time.sleep(wait_before)
        self.send(Permission.ALWAYS_ALLOW)
        time.sleep(2.0)

    def wait_for_idle(self, seconds: float = TURN_COMPLETE_WAIT) -> None:
        """Wait for the AI turn to complete (text streaming + tool execution)."""
        time.sleep(seconds)

    def rewind(self) -> None:
        """Trigger rewind (R) and confirm (y). Must be in Chat focus."""
        self.send(Chat.REWIND)
        time.sleep(1.0)
        self.send(Confirm.YES)
        time.sleep(REWIND_SETTLE)

    def rewind_fork_instead(self) -> None:
        """Trigger rewind (R) and choose fork-instead (f)."""
        self.send(Chat.REWIND)
        time.sleep(1.0)
        self.send(Confirm.FORK_INSTEAD)
        time.sleep(REWIND_SETTLE)

    def rewind_cancel(self) -> None:
        """Trigger rewind (R) and cancel (n)."""
        self.send(Chat.REWIND)
        time.sleep(1.0)
        self.send(Confirm.NO)
        time.sleep(0.5)

    def fork(self) -> None:
        """Trigger fork (f) and confirm (y). Must be in Chat focus."""
        self.send(Chat.FORK)
        time.sleep(1.0)
        self.send(Confirm.YES)
        time.sleep(REWIND_SETTLE)

    def fork_cancel(self) -> None:
        """Trigger fork (f) and cancel (n)."""
        self.send(Chat.FORK)
        time.sleep(1.0)
        self.send(Confirm.NO)
        time.sleep(0.5)

    def open_help(self) -> None:
        """Open help overlay (?). Must be in Chat focus."""
        self.send(Chat.HELP)
        time.sleep(0.5)

    def close_overlay(self) -> None:
        """Close any overlay with Escape."""
        self.send(ESC)
        time.sleep(0.5)

    def open_search(self) -> None:
        """Open within-conversation search (Ctrl+F)."""
        self.send(CTRL_F)
        time.sleep(0.5)

    def toggle_sidebar(self) -> None:
        """Toggle sidebar (Ctrl+H)."""
        self.send(CTRL_H)
        time.sleep(0.5)

    def new_tab(self) -> None:
        """Open a new tab (Ctrl+T)."""
        self.send(CTRL_T)
        time.sleep(0.5)

    def switch_tab(self, n: int) -> None:
        """Switch to tab N (1-9)."""
        assert 1 <= n <= 9
        self.send(str(n))
        time.sleep(0.3)

    def toggle_bookmark(self) -> None:
        """Toggle bookmark on focused message (m). Must be in Chat focus."""
        self.send(Chat.BOOKMARK_TOGGLE)
        time.sleep(0.3)

    def open_bookmark_list(self) -> None:
        """Open bookmark list panel ('). Must be in Chat focus."""
        self.send(Chat.BOOKMARK_LIST)
        time.sleep(0.5)

    # ── File Assertions ──────────────────────────────────────────────────

    def file_path(self, relative: str) -> Path:
        """Resolve a path relative to the workspace."""
        return self.wp / relative

    def file_exists(self, relative: str) -> bool:
        """Check if a file exists in the workspace."""
        return self.file_path(relative).exists()

    def file_content(self, relative: str) -> str:
        """Read file content from workspace. Raises if missing."""
        return self.file_path(relative).read_text()

    def file_content_bytes(self, relative: str) -> bytes:
        """Read file content as bytes from workspace."""
        return self.file_path(relative).read_bytes()

    def write_file(self, relative: str, content: str) -> Path:
        """Write a file into the workspace (for pre-populating test fixtures)."""
        p = self.file_path(relative)
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(content)
        return p

    def remove_file(self, relative: str) -> None:
        """Remove a file from the workspace."""
        p = self.file_path(relative)
        if p.exists():
            p.unlink()

    # ── Log Inspection ───────────────────────────────────────────────────

    def log_lines(self, since_start: bool = True) -> list[str]:
        """Read log lines, optionally only those written since this TUI started."""
        log = _today_log()
        if not log:
            return []
        lines = log.read_text().splitlines()
        if since_start:
            lines = lines[self._log_line_offset :]
        return lines

    def log_contains(self, pattern: str) -> bool:
        """Check if any log line matches a regex pattern."""
        regex = re.compile(pattern)
        return any(regex.search(line) for line in self.log_lines())

    def log_grep(self, pattern: str) -> list[str]:
        """Return all log lines matching a regex pattern."""
        regex = re.compile(pattern)
        return [line for line in self.log_lines() if regex.search(line)]

    def assert_log_contains(self, pattern: str, msg: str = "") -> None:
        """Assert that at least one log line matches the pattern."""
        matches = self.log_grep(pattern)
        assert matches, (
            f"Expected log pattern '{pattern}' not found. {msg}\n"
            f"Log lines ({len(self.log_lines())}):\n"
            + "\n".join(self.log_lines()[-20:])
        )

    def assert_log_not_contains(self, pattern: str, msg: str = "") -> None:
        """Assert that no log line matches the pattern."""
        matches = self.log_grep(pattern)
        assert not matches, (
            f"Unexpected log pattern '{pattern}' found. {msg}\n"
            + "\n".join(matches[:5])
        )

    # ── Session Inspection ───────────────────────────────────────────────

    def sessions_dir(self) -> Path:
        """Path to .claude/sessions/ under the workspace."""
        return self.wp / ".claude" / "sessions"

    def session_ids(self) -> list[str]:
        """List all session IDs in the workspace."""
        sd = self.sessions_dir()
        if not sd.exists():
            return []
        return [d.name for d in sd.iterdir() if d.is_dir()]

    def checkpoints_exist(self, session_id: str | None = None) -> bool:
        """Check if any session has a checkpoints.json file."""
        sd = self.sessions_dir()
        if not sd.exists():
            return False
        if session_id:
            return (sd / session_id / "checkpoints.json").exists()
        return any((sd / sid / "checkpoints.json").exists() for sid in self.session_ids())

    def snapshot_count(self, session_id: str | None = None) -> int:
        """Count snapshot files across sessions (or in a specific one)."""
        sd = self.sessions_dir()
        if not sd.exists():
            return 0
        total = 0
        dirs = [sd / session_id] if session_id else [sd / s for s in self.session_ids()]
        for d in dirs:
            snap_dir = d / "snapshots"
            if snap_dir.exists():
                total += sum(
                    1 for f in snap_dir.iterdir()
                    if f.is_file() and not f.name.endswith(".tmp")
                )
        return total


# ── Convenience Context Manager ──────────────────────────────────────────────

@contextmanager
def rustain_tui(**kwargs) -> Iterator[RustainTUI]:
    """Shorthand context manager for creating and tearing down a TUI session.

    Usage::

        with rustain_tui(fresh=True, build=False) as tui:
            tui.send_message("hello")
    """
    tui = RustainTUI(**kwargs)
    try:
        tui.start()
        yield tui
    finally:
        tui.stop()
