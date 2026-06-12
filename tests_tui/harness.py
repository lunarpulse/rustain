"""RustainTUI — pexpect + pyte harness for E2E TUI testing.

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

import json
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
import pyte

from keys import (
    ESC, ENTER, CTRL_C, CTRL_F, CTRL_H, CTRL_P, CTRL_T,
    Chat, Confirm, Permission, Sidebar,
)

# ── Configuration ────────────────────────────────────────────────────────────

# Project root — one level up from tests_tui/
PROJECT_ROOT = Path(__file__).resolve().parent.parent
BINARY = PROJECT_ROOT / "target" / "debug" / "rustain"
ENV_FILE = PROJECT_ROOT / ".env"
LOG_DIR = Path.home() / ".rustain"

# Terminal dimensions for the spawned PTY.
# TERM_COLS must be >= SIDEBAR_MIN_WIDTH (120, see src/adapters/tui/layout.rs)
# so sidebar-focused tests can actually show the History panel.
TERM_ROWS = 30
TERM_COLS = 130

# Timing defaults (seconds)
STARTUP_WAIT = 3.0
KEY_DELAY = 0.2
PERMISSION_WAIT = 10.0
TOOL_EXEC_WAIT = 30.0
TURN_COMPLETE_WAIT = 30.0
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
    if not BINARY.exists():
        raise RuntimeError(f"Binary not found at {BINARY}")
    return BINARY


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

    allowed_tools: list[str] | None = None
    """Override the AlwaysAllow list in the temp workspace settings.json.
    None → default set (Read, Write, Edit, Bash, Glob, Grep).
    Empty list → no tools pre-allowed (permission prompts for everything).
    """

    timeout: float = 120.0
    """pexpect timeout for expect() calls."""

    env_overrides: dict[str, str] | None = None
    """Extra env vars merged into the spawned process env (last-write-wins).

    Use for per-test settings such as ``RUSTAIN_PROFILE`` — keeps tests
    isolated without mutating ``os.environ`` (xdist-safe).
    """

    _child: pexpect.spawn | None = field(default=None, init=False, repr=False)
    _tmpdir: tempfile.TemporaryDirectory | None = field(
        default=None, init=False, repr=False
    )
    _workspace_path: Path | None = field(default=None, init=False, repr=False)
    _screen: pyte.Screen | None = field(default=None, init=False, repr=False)
    _stream: pyte.Stream | None = field(default=None, init=False, repr=False)

    # ── Lifecycle ────────────────────────────────────────────────────────

    def start(self) -> "RustainTUI":
        """Build (if requested) and spawn the TUI process."""
        if self.build:
            _build_binary()

        # Resolve workspace directory
        if self.workspace is None:
            self._tmpdir = tempfile.TemporaryDirectory(prefix="rustain_test_")
            self._workspace_path = Path(self._tmpdir.name)
        else:
            self._workspace_path = self.workspace

        # Copy .env so the API key is available (skip if already present)
        if ENV_FILE.exists() and not (self._workspace_path / ".env").exists():
            shutil.copy2(ENV_FILE, self._workspace_path / ".env")

        # Pre-create .rustain/config.toml (6-0c format) for
        # session-level auto-allow rules. ApprovalRuntime loads this at
        # startup, so tests control permissions via this file.
        allow_list = (
            self.allowed_tools
            if self.allowed_tools is not None
            else ["Read", "Write", "Edit", "Bash", "Glob", "Grep"]
        )
        rustain_dir = self._workspace_path / ".rustain"
        rustain_dir.mkdir(parents=True, exist_ok=True)
        if not (rustain_dir / "config.toml").exists():
            (rustain_dir / "config.toml").write_text(
                "[permissions]\n"
                + f"always_tools = {json.dumps(allow_list)}\n"
            )
        if not (rustain_dir / "permissions.toml").exists():
            (rustain_dir / "permissions.toml").write_text("")

        # Keep .claude/settings.json for backward compatibility with
        # other code that may still read it (doctor, init, etc.).
        settings_dir = self._workspace_path / ".claude"
        settings_dir.mkdir(parents=True, exist_ok=True)
        if not (settings_dir / "settings.json").exists():
            (settings_dir / "settings.json").write_text(
                json.dumps({"permissions": {"allow": allow_list}}) + "\n"
            )

        env = _load_env()
        # Redirect rustain's config_dir() to the test workspace so
        # ApprovalRuntime::load_session() reads from the test's
        # .rustain/config.toml instead of the developer's ~/.config/rustain/.
        env["RUSTAIN_CONFIG_DIR"] = str(self._workspace_path / ".rustain")
        env["RUSTAIN_DATA_DIR"] = str(self._workspace_path / ".rustain_data")
        # Isolate logs per test (P0 flakiness fix: prevents cross-test log pollution)
        env["RUSTAIN_LOG_PATH"] = str(self._workspace_path / "rustain.log")
        # Ensure RUST_LOG is set to debug for E2E tests since assertions check debug-level logs.
        if "RUST_LOG" not in env:
            env["RUST_LOG"] = "debug"
        # Per-test env overrides (e.g. RUSTAIN_PROFILE for MCP tests).
        if self.env_overrides:
            env.update(self.env_overrides)
        args = [str(BINARY)]
        if self.fresh:
            args.append("--new")

        # Initialize pyte virtual terminal — use Stream (str) since pexpect
        # is configured with encoding="utf-8" which returns str, not bytes.
        self._screen = pyte.Screen(TERM_COLS, TERM_ROWS)
        self._stream = pyte.Stream(self._screen)

        self._child = pexpect.spawn(
            args[0],
            args=args[1:],
            cwd=str(self._workspace_path),
            env=env,
            timeout=self.timeout,
            encoding="utf-8",
            dimensions=(TERM_ROWS, TERM_COLS),
        )

        # Wait for TUI to be fully ready (status bar shows "Ready" in idle state)
        # instead of a fixed sleep. Falls back gracefully if polling times out.
        if not self.wait_for_screen("Ready", timeout=STARTUP_WAIT * 5):
            # Brief fallback pause if we couldn't detect Ready within 15s
            time.sleep(0.5)
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
        if self._child is None:
            raise RuntimeError("TUI not started — call .start() first")
        return self._child

    @property
    def wp(self) -> Path:
        """Workspace path."""
        if self._workspace_path is None:
            raise RuntimeError("Workspace path not set — TUI not started")
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

    # ── pyte Screen Integration ──────────────────────────────────────────

    def _sync_screen(self) -> None:
        """Drain all available PTY output into the pyte screen buffer.

        Pattern: drain-then-assert. Call before any screen content check.
        Uses read_nonblocking() to avoid blocking the test thread.
        """
        if self._stream is None:
            raise RuntimeError("pyte stream not initialized — call .start() first")
        if self._child is None or not self._child.isalive():
            raise RuntimeError("TUI process is not alive — cannot sync screen")
        while True:
            try:
                data = self.child.read_nonblocking(size=4096, timeout=0.1)
                if data:
                    self._stream.feed(data)
                else:
                    break
            except (pexpect.TIMEOUT, pexpect.EOF):
                break
            except UnicodeDecodeError as exc:
                raise RuntimeError(
                    f"UnicodeDecodeError feeding pyte stream: {exc}"
                ) from exc

    def get_screen_text(self) -> str:
        """Return the full terminal screen content as a single string.

        Drains the PTY buffer into pyte before reading. Lines are joined
        with newlines. pyte strips ANSI styling — only plain text is returned.
        """
        self._sync_screen()
        if self._screen is None:
            raise RuntimeError("pyte screen not initialized — call .start() first")
        return "\n".join(self._screen.display)

    def assert_screen_contains(self, text: str, msg: str = "") -> None:
        """Assert that ``text`` appears anywhere on the current screen.

        Drains the PTY buffer first, then checks the rendered terminal state.
        Assert on **key visible text**, not exact layout or ANSI codes.
        """
        screen_text = self.get_screen_text()
        assert text in screen_text, (
            f"Expected '{text}' on screen. {msg}\n"
            f"Screen content:\n{screen_text}"
        )

    def assert_screen_not_contains(self, text: str, msg: str = "") -> None:
        """Assert that ``text`` does NOT appear anywhere on the current screen."""
        screen_text = self.get_screen_text()
        assert text not in screen_text, (
            f"Expected '{text}' NOT on screen. {msg}\n"
            f"Screen content:\n{screen_text}"
        )

    def is_stream_disconnected(self) -> bool:
        """Check whether the screen shows an API stream disconnect message.

        When the provider rate-limits or drops the SSE stream, the TUI shows
        'Stream disconnected unexpectedly' and an '[interrupted]' tag. Tests
        that depend on tool execution should skip gracefully in this state.
        """
        screen = self.get_screen_text()
        return "Stream disconnected" in screen or "interrupted" in screen

    def wait_for_screen(
        self,
        text: str,
        timeout: float = TURN_COMPLETE_WAIT,
        poll_interval: float = 0.5,
    ) -> bool:
        """Poll the pyte screen buffer until ``text`` appears or timeout.

        Returns ``True`` if text found within timeout, ``False`` on timeout.
        Prefer this over ``wait_for_idle()`` when expected screen content is
        deterministic (UI state changes, overlay open/close, startup).
        Keep ``wait_for_idle()`` for unpredictable AI response content.
        """
        if not text:
            raise ValueError("wait_for_screen: text must not be empty")
        if poll_interval <= 0:
            raise ValueError(f"wait_for_screen: poll_interval must be positive, got {poll_interval}")
        elapsed = 0.0
        while elapsed < timeout:
            if text in self.get_screen_text():
                return True
            time.sleep(poll_interval)
            elapsed += poll_interval
        return False

    def wait_for_screen_not_contains(
        self,
        text: str,
        timeout: float = 5.0,
        poll_interval: float = 0.5,
    ) -> bool:
        """Poll the pyte screen buffer until ``text`` disappears or timeout.

        Returns ``True`` if text disappeared within timeout, ``False`` on timeout.
        Use for overlay dismissal, sidebar close, and other negative conditions
        where a fixed ``wait(0.3)`` is racy on loaded CI runners.
        """
        if not text:
            raise ValueError("wait_for_screen_not_contains: text must not be empty")
        if poll_interval <= 0:
            raise ValueError(f"wait_for_screen_not_contains: poll_interval must be positive, got {poll_interval}")
        elapsed = 0.0
        while elapsed < timeout:
            if text not in self.get_screen_text():
                return True
            time.sleep(poll_interval)
            elapsed += poll_interval
        return False

    def assert_responsive(self, timeout: float = 3.0) -> None:
        """Verify the TUI process is alive AND accepting input.

        Types a single character into the input box, then polls for the
        character to appear on the pyte screen.  Uses ``x`` as the probe
        character because it is unambiguous (won't trigger autocomplete
        or overlays) and clears it afterwards via Backspace.

        Raises ``RuntimeError`` if the process is dead, ``TimeoutError`` if
        alive but unresponsive.
        """
        if self._child is None:
            raise RuntimeError("TUI not started — call .start() first")
        if not self._child.isalive():
            raise RuntimeError("TUI process is not alive")

        screen_before = self.get_screen_text()

        self._child.send("x")
        time.sleep(0.1)
        self._child.send("\x7f")
        time.sleep(0.1)

        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            time.sleep(0.05)
            if not self._child.isalive():
                raise RuntimeError("TUI process died during responsiveness check")
            screen_after = self.get_screen_text()
            if screen_after != screen_before:
                return

        raise TimeoutError(
            f"TUI did not respond to probe within {timeout}s — likely frozen"
        )

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

    def session_allow_permission(
        self, wait_before: float = PERMISSION_WAIT
    ) -> None:
        """Press 's' to session-allow the tool permission (AC4).

        Auto-approves all queued requests for the same tool type.
        Not persisted — only lasts for the process lifetime.
        """
        time.sleep(wait_before)
        self.send(Permission.SESSION_ALLOW)
        self.wait_for_screen_not_contains("[y] Allow", timeout=3.0)

    def deny_with_feedback(
        self, feedback: str, wait_before: float = PERMISSION_WAIT
    ) -> None:
        """Press 'f', type feedback text, and submit (AC5).

        Opens the deny-with-feedback mini-input, types the given text,
        presses Enter, and asserts the feedback block appears.
        """
        time.sleep(wait_before)
        self.send(Permission.DENY_FEEDBACK)
        self.wait_for_screen("Feedback:", timeout=3.0)
        for ch in feedback:
            self.send(ch)
            time.sleep(0.05)
        self.send(ENTER)
        self.wait_for_screen(
            f'Tool denied. User feedback: "{feedback}"', timeout=5.0
        )

    def set_permission_mode(self, mode: str) -> None:
        """Switch permission mode via Ctrl+P → mode: <mode> (AC9).

        Valid modes: plan, normal, autoedit, yolo.

        Waits for the status-bar flash emitted by the SetPermissionMode handler
        (``Permission mode: <Mode>`` for plan/normal/autoedit, or the YOLO
        warning for yolo) so the test fails loudly if the mode actually did not
        change — previously this just waited for ``mode.lower()`` anywhere on
        screen, which could match unrelated text.
        """
        self.send(CTRL_P)
        time.sleep(0.3)
        for ch in f"mode: {mode}":
            self.send(ch)
            time.sleep(0.05)
        self.send(ENTER)
        arg = mode.lower()
        expected = {
            "plan": "Permission mode: Plan",
            "normal": "Permission mode: Normal",
            "autoedit": "Permission mode: AutoEdit",
            "auto": "Permission mode: AutoEdit",
            "yolo": "YOLO mode active",
        }.get(arg)
        if expected is None:
            raise ValueError(f"Unknown permission mode: {mode}")
        self.wait_for_screen(expected, timeout=3.0)

    def wait_for_idle(self, seconds: float = TURN_COMPLETE_WAIT) -> None:
        """Wait for the AI turn to complete (text streaming + tool execution).

        Polls the screen for the "Ready" status indicator which appears when
        the turn finishes. Falls back to a fixed sleep if polling fails, so
        existing tests that pass arbitrary ``seconds`` continue to work.
        """
        if not self.wait_for_screen("Ready", timeout=seconds):
            time.sleep(1.0)

    def rewind(self) -> None:
        """Trigger rewind (R) and confirm (y). Must be in Chat focus."""
        self.send(Chat.REWIND)
        self.wait_for_screen("Rewind", timeout=5.0)
        self.send(Confirm.YES)
        self.wait_for_screen("Rewound to message", timeout=REWIND_SETTLE * 2)

    def rewind_fork_instead(self) -> None:
        """Trigger rewind (R) and choose fork-instead (f)."""
        self.send(Chat.REWIND)
        self.wait_for_screen("Rewind", timeout=5.0)
        self.send(Confirm.FORK_INSTEAD)
        self.wait_for_screen_not_contains("Rewind", timeout=REWIND_SETTLE)

    def rewind_cancel(self) -> None:
        """Trigger rewind (R) and cancel (n)."""
        self.send(Chat.REWIND)
        self.wait_for_screen("Rewind", timeout=5.0)
        self.send(Confirm.NO)
        self.wait_for_screen_not_contains("Rewind", timeout=2.0)

    def fork(self) -> None:
        """Trigger fork (f) and confirm (y). Must be in Chat focus."""
        self.send(Chat.FORK)
        self.wait_for_screen("Fork", timeout=5.0)
        self.send(Confirm.YES)
        self.wait_for_screen("Forked conversation", timeout=REWIND_SETTLE * 2)

    def fork_cancel(self) -> None:
        """Trigger fork (f) and cancel (n)."""
        self.send(Chat.FORK)
        self.wait_for_screen("Fork", timeout=5.0)
        self.send(Confirm.NO)
        self.wait_for_screen_not_contains("Fork", timeout=2.0)

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
        self.send(ESC)
        self.send(CTRL_F)
        time.sleep(0.5)

    def toggle_sidebar(self) -> None:
        """Toggle sidebar via command palette.

        Uses the palette route ("toggle sidebar" → Enter) instead of the raw
        Ctrl+H byte (\x08) which crossterm decodes as KeyCode::Backspace in a
        standard PTY — preventing the ToggleSidebar action from firing.
        """
        self.send(CTRL_P)
        self.wait_for_screen("Command Palette", timeout=3.0)
        for c in "toggle sidebar":
            self.send(c)
            time.sleep(0.05)
        self.wait_for_screen("toggle sidebar", timeout=2.0)
        self.send(ENTER)
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

    def close_tab(self) -> None:
        """Close the active tab via the command palette (close tab action)."""
        self.send(CTRL_P)
        self.wait_for_screen("Command Palette", timeout=3.0)
        for c in "close tab":
            self.send(c)
            time.sleep(0.05)
        self.wait_for_screen("close tab", timeout=2.0)
        self.send(ENTER)
        time.sleep(0.5)

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

    # ── Export Assertions ────────────────────────────────────────────────

    def _exports_dir(self) -> Path:
        """.rustain/exports/ subdirectory under the workspace."""
        return self.wp / ".rustain" / "exports"

    def export_file_exists(self, filename: str) -> bool:
        """Check if an exported file exists under .rustain/exports/.

        /export <name> writes to {workspace}/.rustain/exports/<name>,
        NOT to the workspace root.
        """
        return (self._exports_dir() / filename).exists()

    def wait_for_export_file(
        self, filename: str, timeout: float = 10.0, poll_interval: float = 0.25
    ) -> bool:
        """Poll .rustain/exports/<filename> until it exists or timeout.

        Prefer this over a fixed ``wait(...)`` after /export — the write is
        routed through ``spawn_blocking`` and may trail the command submit
        by more than a couple of seconds under load. Returns True on found.
        """
        path = self._exports_dir() / filename
        elapsed = 0.0
        while elapsed < timeout:
            if path.exists():
                return True
            time.sleep(poll_interval)
            elapsed += poll_interval
        return False

    def export_file_content(self, filename: str) -> str:
        """Read the content of an exported file from .rustain/exports/."""
        return (self._exports_dir() / filename).read_text()

    # ── Log Inspection ───────────────────────────────────────────────────

    def log_lines(self, since_start: bool = True) -> list[str]:
        """Read log lines from the test-isolated log file.

        The ``since_start`` parameter is retained for backward compatibility
        but is a no-op because each test has its own log file (P0 flakiness fix).
        """
        from datetime import date
        log = self._workspace_path / f"rustain.log.{date.today()}"
        if not log.exists():
            return []
        return log.read_text().splitlines()

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
        """List all session IDs in the workspace.

        Handles both storage layouts:
        - Directory layout (with images): {sessions_dir}/{id}/ directory
        - Flat layout (text-only):        {sessions_dir}/{id}.meta.json file
        """
        sd = self.sessions_dir()
        if not sd.exists():
            return []
        ids: set[str] = set()
        for entry in sd.iterdir():
            if entry.is_dir():
                ids.add(entry.name)                          # directory layout
            elif entry.name.endswith(".meta.json"):
                ids.add(entry.name[: -len(".meta.json")])   # flat layout
        return list(ids)

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
