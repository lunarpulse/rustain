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
from typing import Iterator, NamedTuple

import pexpect
import pyte
from pyte import modes as terminal_modes
from wcwidth import wcwidth

from keys import (
    ESC, ENTER, CTRL_C, CTRL_F, CTRL_H, CTRL_P, CTRL_T, CTRL_X,
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
# so the read-only monitor column (Ctrl+X w) can actually show.
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


def _resolve_binary() -> tuple[Path, bool]:
    """Resolve the binary to spawn: ``RUSTAIN_TUI_BINARY`` when set, else the
    debug build. Returns ``(path, overridden)``.

    Read at every ``start()`` rather than at import, so a test can point one
    harness instance at a release-profile binary (Story 19.7 AC3) without
    mutating the module or the process env of its siblings.
    """
    override = os.environ.get("RUSTAIN_TUI_BINARY", "").strip()
    if override:
        # Resolve against the CALLER's cwd: the child is spawned with
        # cwd=<workspace>, so a relative override that passed the existence
        # check here would be unfindable at spawn time.
        return Path(override).resolve(), True
    return BINARY, False


class TerminalScreen(pyte.Screen):
    """Model replacement of either half of a wide terminal glyph.

    pyte 0.8 leaves the other half behind when a cursor-addressed repaint
    overwrites a wide glyph. Real terminals erase it; ratatui's incremental
    diff relies on that behavior instead of emitting redundant blank cells.
    An orphaned pyte continuation also makes its display property raise.
    Repair the neighboring cells at write time, not the observed screen text.
    """

    def draw(self, data: str) -> None:
        for character in data:
            translated = character.translate(
                self.g1_charset if self.charset else self.g0_charset)
            width = wcwidth(translated)
            x = self.cursor.x
            if x == self.columns:
                x = 0 if terminal_modes.DECAWM in self.mode else x - max(width, 0)
            super().draw(character)
            if width > 0:
                line = self.buffer[self.cursor.y]
                if x > 0 and line[x - 1].data and wcwidth(line[x - 1].data[0]) == 2:
                    line[x - 1] = line[x - 1]._replace(data=" ")
                end = x + width
                if end < self.columns and line[end].data == "":
                    line[end] = line[end]._replace(data=" ")


# ── Notification screen helpers (Story 21.1; strip-aware since Story 21.7) ───
#
# Positional predicates over a rendered screen (``"\n".join(pyte.display)``):
# the composer is the bottom-most bordered box, the status row sits directly
# above it, the mission strip (one muted row, Story 21.7 A22) sits directly
# above the status row whenever the chat pane can spare it, and the chat pane's
# bottom edge — where toasts are painted, right-aligned, newest at the bottom —
# is the row(s) directly above that. Nothing here is an unanchored substring
# search.

NOTIFICATION_LOG_TITLE = "┌ System · Notifications "
_SEVERITY_WORDS = ("Info", "Success", "Warning", "Error")
_LOG_ROW = re.compile(r"^(?:Info|Success|Warning|Error) · \d{2}:\d{2}:\d{2} · ")


def composer_top(lines: list[str]) -> int | None:
    """Index of the composer's top border: the nearest ``┌`` above the
    bottom-most ``└``."""
    bottom = max((i for i, line in enumerate(lines) if line.strip().startswith("└")), default=None)
    if bottom is None:
        return None
    return max((i for i in range(bottom) if lines[i].strip().startswith("┌")), default=None)


_STRIP_SEGMENT = (
    r"(?:(?:Agents|A) \d+/\d+|(?:Tasks|T) \d+/\d+|(?:Decisions|D|Writer decisions|W) \d+)"
)
_STRIP_HINT = r"\^X \S+ (?:Mission Control|Messages)"
# The A22 grammar, anchored to the row start; a monitor column painted to the
# right of the main column may follow after at least two blanks.
_STRIP_ROW = re.compile(
    rf"^(?:{_STRIP_SEGMENT}(?: · {_STRIP_SEGMENT})*|{_STRIP_HINT})(?:\s{{2,}}\S.*)?$"
)


def status_row(screen: str) -> str:
    """The status bar row: the one directly above the composer box ('' if none).

    The mission strip is above this row, never between it and the composer."""
    lines = screen.split("\n")
    top = composer_top(lines)
    return lines[top - 1] if top else ""


def strip_row(screen: str) -> str:
    """The mission strip row: the row directly above the status row when it
    matches the A22 grammar (``Agents r/n · Tasks d/n · Decisions n``, the
    abbreviated ``A/T/D`` form, ``Writer decisions n``, or the idle key hint);
    '' when the strip yielded its row to the Stage or is absent."""
    lines = screen.split("\n")
    top = composer_top(lines)
    if not top or top < 2:
        return ""
    row = lines[top - 2].rstrip()
    return row if _STRIP_ROW.match(row) else ""


def toast_rows(screen: str, rows: int = 1) -> list[str]:
    """The chat-pane bottom rows directly above the status row (and above the
    mission strip when one is painted), newest first.

    One borderless row per visible toast below 100 columns (the newest toast is
    the row adjacent to the strip, or to the status row when the strip yielded)."""
    lines = screen.split("\n")
    top = composer_top(lines)
    if not top:
        return []
    first = top - 2 - (1 if strip_row(screen) else 0)
    return [lines[first - k] for k in range(rows) if first - k >= 0]


def toast_present(screen: str, text: str, severity: str | None = None, rows: int = 1) -> bool:
    """True when one of the ``rows`` toast rows ends with ``<Severity> <text>``
    (optionally followed by `` ×N`` and, for the bordered toast of a pane of
    100+ columns, its right border): right-aligned, anchored at the row end,
    severity word first. ``text`` is the painted first line (with ``…`` when the
    message has more lines)."""
    words = "|".join(re.escape(w) for w in ([severity] if severity else _SEVERITY_WORDS))
    pattern = re.compile(rf"(?<![A-Za-z0-9])(?:{words}) {re.escape(text)}(?: ×\d+)?\s*│?$")
    return any(pattern.search(row.rstrip()) for row in toast_rows(screen, rows))


_MODE_SEGMENT = re.compile(r"│ (Normal|PLAN|AUTOEDIT|YOLO)(?: │|\s*$)")
_LOG_ROW_TIME = re.compile(r"^(?:Info|Success|Warning|Error) · (\d{2}):(\d{2}):(\d{2}) · ")


def mode_segment(screen: str) -> str | None:
    """The status row's permission-mode segment (``Normal``, ``PLAN``,
    ``AUTOEDIT``, ``YOLO``), read positionally; None when the row shows none."""
    match = _MODE_SEGMENT.search(status_row(screen))
    return match.group(1) if match else None


_MARK_READ_BUDGET = 0.3  # seconds before the turn from which a mark only drains the PTY in short reads
_MARK_SLACK = 0.05  # the most a mark may return after the turn it aligned to


class NotificationMark(NamedTuple):
    """A point in time for ``wait_for_notification``: the whole wall-clock second
    the mark returned at."""

    second: int


def _log_row_at_or_after(row: str, mark_second: int) -> bool:
    """True when the S-LOG row's local ``HH:MM:SS`` is at or after the mark's
    second. Both are whole seconds of the same local clock; the comparison is
    modular over one day, so it holds across midnight."""
    match = _LOG_ROW_TIME.match(row)
    if match is None:
        return False
    hours, minutes, seconds = (int(part) for part in match.groups())
    mark = time.localtime(mark_second)
    ahead = (hours * 3600 + minutes * 60 + seconds
             - (mark.tm_hour * 3600 + mark.tm_min * 60 + mark.tm_sec)) % 86400
    return ahead < 43200


def _log_row_names(row: str, text: str) -> bool:
    """True when the S-LOG row carries ``text``: whole, or as the clipped prefix
    (at least 20 characters, then ``…``) the list paints when the message is
    wider than the box."""
    if text in row:
        return True
    clipped = row[:-1] if row.endswith("…") else None
    return clipped is not None and any(text.startswith(clipped[i:]) for i in range(len(clipped) - 19))


def workspace_config_text(allow_list: list[str], notification_ttl_secs: int | None = None) -> str:
    """The harness workspace's ``.rustain/config.toml``; the optional
    ``[notifications]`` table lifts the Info and Success lifetimes."""
    text = "[permissions]\n" + f"always_tools = {json.dumps(allow_list)}\n"
    if notification_ttl_secs is not None:
        text += (f"\n[notifications]\ninfo_ttl_secs = {notification_ttl_secs}\n"
                 f"success_ttl_secs = {notification_ttl_secs}\n")
    return text


def _notification_log_box(screen: str) -> list[str]:
    """Inner text rows of the ``System · Notifications`` frame ([] when closed)."""
    lines = screen.split("\n")
    top = next((i for i, line in enumerate(lines) if NOTIFICATION_LOG_TITLE in line), None)
    if top is None:
        return []
    left = lines[top].index(NOTIFICATION_LOG_TITLE)
    right = lines[top].rfind("┐")
    if right <= left:
        return []
    bottom = next((i for i in range(top + 1, len(lines))
                   if len(lines[i]) > right and lines[i][left] == "└"), len(lines))
    return [lines[i][left + 1:right] for i in range(top + 1, bottom)]


def notification_log_rows(screen: str) -> list[str]:
    """S-LOG list rows (``Severity · HH:MM:SS · source · …``), newest first."""
    rows = [row.strip() for row in _notification_log_box(screen)]
    return [row for row in rows if _LOG_ROW.match(row)]


def notification_log_text(screen: str) -> str:
    """Whitespace-normalised text of the whole S-LOG frame interior."""
    return " ".join(" ".join(_notification_log_box(screen)).split())


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

    extra_args: list[str] | None = None
    """Arguments appended after the binary (and ``--new``), e.g.
    ``["daemon", "attach"]`` to drive the daemon-attached client."""
    binary: Path | None = None
    """Per-process binary override; defaults to RUSTAIN_TUI_BINARY/build behavior."""

    notification_ttl_secs: int | None = None
    """Info and Success toast lifetime written to the workspace config. None keeps
    the product default (4 s); set it (e.g. 30) only where the toast row itself is
    the claim, and dismiss the toast with ``Ctrl+X x``."""

    _child: pexpect.spawn | None = field(default=None, init=False, repr=False)
    _tmpdir: tempfile.TemporaryDirectory | None = field(
        default=None, init=False, repr=False
    )
    _workspace_path: Path | None = field(default=None, init=False, repr=False)
    _screen: pyte.Screen | None = field(default=None, init=False, repr=False)
    _stream: pyte.Stream | None = field(default=None, init=False, repr=False)
    _binary: Path | None = field(default=None, init=False, repr=False)

    # ── Lifecycle ────────────────────────────────────────────────────────

    def start(self) -> "RustainTUI":
        """Build (if requested) and spawn the TUI process."""
        # A named binary is spawned as named: no build, and no silent fallback to
        # target/debug — a missing path is the caller's mistake, said out loud
        # before cargo runs and before pexpect turns it into its own error.
        if self.binary is not None:
            self._binary, overridden = self.binary.resolve(), True
        else:
            self._binary, overridden = _resolve_binary()
        if overridden:
            if not self._binary.is_file():
                source = f"binary={self._binary}" if self.binary is not None else f"RUSTAIN_TUI_BINARY={self._binary}"
                raise FileNotFoundError(f"{source} does not exist")
        elif self.build:
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
        if self.notification_ttl_secs is not None and (rustain_dir / "config.toml").exists():
            raise ValueError("notification_ttl_secs needs a harness-written config.toml; "
                             "this workspace already has one")
        if not (rustain_dir / "config.toml").exists():
            (rustain_dir / "config.toml").write_text(
                workspace_config_text(allow_list, self.notification_ttl_secs))
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
        args = [str(self._binary)]
        if self.fresh:
            args.append("--new")
        if self.extra_args:
            args.extend(self.extra_args)

        # Initialize pyte virtual terminal — use Stream (str) since pexpect
        # is configured with encoding="utf-8" which returns str, not bytes.
        self._screen = TerminalScreen(TERM_COLS, TERM_ROWS)
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
        deadline = time.monotonic() + timeout
        while True:
            if text in self.get_screen_text():
                return True
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return False
            time.sleep(min(poll_interval, remaining))

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
        deadline = time.monotonic() + timeout
        while True:
            if text not in self.get_screen_text():
                return True
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return False
            time.sleep(min(poll_interval, remaining))

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

        Waits for the confirmation the SetPermissionMode handler produces: the
        4 s Info toast ``Permission mode: <Mode>`` (painted at the bottom edge
        of the chat pane) or — the durable fact — the status row's mode segment
        (``PLAN`` / ``Normal`` / ``AUTOEDIT`` / ``YOLO``). The status-row fact
        is accepted because an identical toast arriving within 5 minutes of the
        previous one's expiry is coalesced into the log and never repainted
        (Story 21.1 A4), so repeating a mode must not depend on the toast. For
        yolo the status fact ``YOLO mode active`` is also accepted. The 3 s
        wait stays inside the 4 s Info TTL; like before it does not raise.
        """
        self.send(CTRL_P)
        time.sleep(0.3)
        for ch in f"mode: {mode}":
            self.send(ch)
            time.sleep(0.05)
        self.send(ENTER)
        arg = mode.lower()
        toast = {
            "plan": "Permission mode: Plan",
            "normal": "Permission mode: Normal",
            "autoedit": "Permission mode: AutoEdit",
            "auto": "Permission mode: AutoEdit",
            "yolo": "Permission mode: YOLO",
        }.get(arg)
        if toast is None:
            raise ValueError(f"Unknown permission mode: {mode}")
        segment = {"plan": "PLAN", "normal": "Normal", "autoedit": "AUTOEDIT",
                   "auto": "AUTOEDIT", "yolo": "YOLO"}[arg]
        deadline = time.monotonic() + 3.0
        while time.monotonic() < deadline:
            screen = self.get_screen_text()
            # Positional: the Info toast rows above the status row (three rows
            # per bordered toast), or the status row's own facts — never a
            # whole-screen substring a transcript line could satisfy.
            if (toast_present(screen, toast, "Info", rows=9) or mode_segment(screen) == segment
                    or (arg == "yolo" and "YOLO mode active" in status_row(screen))):
                return

    def wait_for_idle(self, seconds: float = TURN_COMPLETE_WAIT) -> None:
        """Wait for the AI turn to complete (text streaming + tool execution).

        Polls the screen for the "Ready" status indicator which appears when
        the turn finishes. Falls back to a fixed sleep if polling fails, so
        existing tests that pass arbitrary ``seconds`` continue to work.
        """
        if not self.wait_for_screen("Ready", timeout=seconds):
            time.sleep(1.0)

    # ── Notifications (Story 21.1) ───────────────────────────────────────

    def wait_until(self, predicate, label: str, timeout: float = 10.0) -> str:
        """Poll the screen until ``predicate(screen)`` holds; return that screen.

        Structural wait: paced by the PTY read inside ``get_screen_text``, never
        by a sleep, and bounded by ``timeout`` (fails with the last screen).
        """
        deadline = time.monotonic() + timeout
        while True:
            screen = self.get_screen_text()
            if predicate(screen):
                return screen
            assert time.monotonic() < deadline, f"Timed out waiting for {label}:\n{screen}"

    def open_notification_log(self, timeout: float = 10.0) -> str:
        """Open S-LOG with ``Ctrl+X`` then ``n`` (Input focus); return the screen
        once the ``System · Notifications`` frame is painted."""
        self.send(CTRL_X)
        self.wait_until(
            lambda s: any(line.strip().startswith("┌") and "Ctrl+X" in line
                          for line in s.split("\n")),
            "the Ctrl+X leader menu", timeout)
        self.send("n")
        # The rail is the frame's last row: waiting for it means the whole frame
        # arrived (PTY output can reach the screen mid-frame).
        return self.wait_until(
            lambda s: NOTIFICATION_LOG_TITLE in s and "[Esc] Close" in notification_log_text(s),
            "S-LOG", timeout)

    def read_notification_detail(self, row_contains: str, timeout: float = 10.0) -> str:
        """In a freshly opened S-LOG (selection on the newest row), select the
        row whose list text contains ``row_contains`` and press Enter; return the
        whitespace-normalised text of the detail frame (the FULL wrapped
        message). Callers close the log with ``close_notification_log``."""
        screen = self.wait_until(
            lambda s: any(row_contains in row for row in notification_log_rows(s)),
            f"S-LOG row containing {row_contains!r}", timeout)
        rows = notification_log_rows(screen)
        index = next(i for i, row in enumerate(rows) if row_contains in row)
        if index:
            self.send("j" * index)
        self.send(ENTER)
        screen = self.wait_until(
            lambda s: "· detail" in notification_log_text(s) and "[Esc] Close" in notification_log_text(s),
            "the S-LOG detail view", timeout)
        return notification_log_text(screen)

    def close_notification_log(self, timeout: float = 10.0) -> None:
        """Close S-LOG (list or detail) with Esc and wait for the frame to go."""
        self.send(ESC)
        self.wait_until(lambda s: NOTIFICATION_LOG_TITLE not in s, "S-LOG closed", timeout)

    def _wait_pty(self, seconds: float) -> None:
        """Feed the screen with whatever the PTY delivers within ``seconds`` (a
        timeout-based read, returning early on data): a bounded wait on the
        terminal, not a sleep."""
        try:
            data = self.child.read_nonblocking(size=65536, timeout=seconds)
        except (pexpect.TIMEOUT, pexpect.EOF):
            return
        if data:
            self._stream.feed(data)

    def notification_mark(self) -> NotificationMark:
        """A mark for ``wait_for_notification``, taken just before the key that
        raises the notice: the whole wall-clock second ``S`` the mark returns at.

        The S-LOG row shows a record's time truncated to the second, so a record
        raised before the mark returns shows a time before ``S`` and one raised
        after the caller's key shows ``S`` or later. Reads the PTY until the last
        ``_MARK_READ_BUDGET`` seconds before the turn, then drains it in 5 ms
        timeout reads (no sleep) and returns at the turn. If a drain overshoots
        the turn by more than ``_MARK_SLACK`` (a slow read), the boundary would
        be older than the second of return: it aligns to the next turn instead.
        The residual window is at most ``_MARK_SLACK`` between the turn and the
        return: a matching notice raised there counts as new."""
        while True:
            boundary = int(time.time()) + 1
            while time.time() < boundary:
                if boundary - time.time() > _MARK_READ_BUDGET:
                    self.get_screen_text()
                else:
                    self._wait_pty(min(boundary - time.time(), 0.005))
            returned = time.time()
            if returned - boundary <= _MARK_SLACK:
                return NotificationMark(int(returned))

    def _confirm_log_detail(self, index: int, text: str, timeout: float) -> bool:
        """In a freshly opened S-LOG list, select row ``index`` (newest first),
        open its Details and report whether the FULL message holds ``text``;
        Esc closes the log."""
        if index:
            self.send("j" * index)
        self.send(ENTER)
        screen = self.wait_until(
            lambda s: "· detail" in notification_log_text(s) and "[Esc] Close" in notification_log_text(s),
            "the S-LOG detail view", timeout)
        found = " ".join(text.split()) in notification_log_text(screen)
        self.close_notification_log(timeout)
        return found

    def wait_for_notification(
        self,
        text: str,
        mark: NotificationMark,
        severity: str | None = None,
        timeout: float = 10.0,
    ) -> str:
        """Wait until the Notification Log (S-LOG) lists a notice containing
        ``text`` with the requested severity and a time at or after the mark's
        second; returns ``"log"``, raises ``AssertionError`` on timeout.

        The log is append-only and timestamped, so it is the only freshness
        authority. A toast is never evidence here: a short-lived toast can paint
        and expire between two reads, and one painted late may be an older
        notice. A row the list clipped (a prefix, then ``…``) is only a
        candidate: its Details must hold the requested text."""
        deadline = time.monotonic() + timeout

        def candidates(screen):
            return [i for i, row in enumerate(notification_log_rows(screen))
                    if row.startswith(severity or "") and _log_row_names(row, text)
                    and _log_row_at_or_after(row, mark.second)]

        self.open_notification_log(timeout=max(deadline - time.monotonic(), 1.0))
        open_log = True
        try:
            screen = self.wait_until(
                lambda s: bool(candidates(s)),
                f"an S-LOG row {text!r} at or after the mark", max(deadline - time.monotonic(), 0.0))
            rows = notification_log_rows(screen)
            found = [i for i in candidates(screen) if text in rows[i]]
            if not found:  # only clipped rows: confirm each in its Details
                for position, index in enumerate(candidates(screen)):
                    if position:
                        self.open_notification_log(timeout=max(deadline - time.monotonic(), 1.0))
                    open_log = False
                    if self._confirm_log_detail(index, text, max(deadline - time.monotonic(), 1.0)):
                        return "log"
                    open_log = False
                raise AssertionError(f"no S-LOG row's Details holds {text!r}:\n{screen}")
        finally:
            if open_log:
                self.close_notification_log()
        return "log"

    def rewind(self) -> None:
        """Trigger rewind (R) and confirm (y). Must be in Chat focus."""
        self.send(Chat.REWIND)
        self.wait_for_screen("Rewind", timeout=5.0)
        self.send(Confirm.YES)
        # `Rewound to message N…` is a 4 s Success toast (Story 21.1); the wait
        # polls from the confirming `y`, so it is caught as soon as it paints.
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
        # The Success toast reads `Forked from "<title>" at message N` (or
        # `Forked at last message — …`), a 4 s toast: wait for its stable prefix.
        self.wait_for_screen("Forked", timeout=REWIND_SETTLE * 2)

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

    def open_history(self) -> None:
        """Open the History window with the leader chord ``Ctrl+X h``.

        Story 21.3: the chord is the portable route (``Ctrl+X`` = ``0x18``, then
        ``h``). Raw ``0x08`` is **Ctrl+H** in crossterm 0.29 (Backspace is
        ``0x7F``), and Ctrl+H is unbound outside text fields now, so it never
        opens anything. Story 21.7: History is a frame over the Stage at every
        width (no sidebar column, no density), so nothing but the chord is needed.

        Both waits are structural: the leader menu's ``[Esc] Cancel`` item, then
        the frame's own box title ``┌History`` (the menu paints a ``[H] History``
        item with no ``┌`` before it, so the menu alone never satisfies it).
        """
        self.send(CTRL_X)
        self.wait_for_screen("[Esc] Cancel", timeout=3.0)
        self.send("h")
        assert self.wait_for_screen("┌History", timeout=5.0), (
            f"History frame did not open after Ctrl+X h. Screen:\n{self.get_screen_text()}"
        )

    def close_history(self) -> None:
        """Close the History window: ``Esc`` goes back one frame (the chord is
        an opener only), then wait for its box title to leave the screen."""
        self.send(ESC)
        assert self.wait_for_screen_not_contains("┌History", timeout=5.0), (
            f"History frame did not close after Esc. Screen:\n{self.get_screen_text()}"
        )

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
        deadline = time.monotonic() + timeout
        while True:
            if path.exists():
                return True
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return False
            time.sleep(min(poll_interval, remaining))

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
