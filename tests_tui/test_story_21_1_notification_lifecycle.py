"""Story 21.1 K12 (S1 leg): 80x24 native notification-lifecycle smoke on a DEBUG binary.

Default-feature, non-API (no `requires_api`, no new marker): a loopback
Anthropic-shaped provider answers every request, so no credential or network
is used; the isolated HOME carries `[notifications] info_ttl_secs = 1` and
`warning_ttl_secs = 1` so a toast lives one second instead of 4 s / 10 s. The
single scenario drives the real binary under a PTY at 80x24:

* local TUI  - an Info notice (`Permission mode: <Mode>` from Shift+Tab) and a
  Warning notice (the Advisory for an unknown `/nope`) appear as ONE borderless,
  right-aligned toast row at the chat pane's bottom edge (the row directly above
  the status row, starting with the severity word) and then disappear on their
  own; the `Log !N` badge shows after the Warning expired; `Ctrl+X n` opens the
  Notification Log (title ` System · Notifications `, `Notifications (N)`,
  newest-first `Severity · HH:MM:SS · source · message` rows, the S-LOG rail),
  clears the badge, `Enter` shows the FULL message and `Esc` closes it.
* attached client - the same toast -> expiry -> Notification Log path for an
  attached Info notice (an unavailable chord's remedy), no wire change.

Every wait is a bounded structural screen predicate (no sleeps); PTY output is
read in small chunks so a one-second toast cannot hide between two reads.
Evidence (screens, raw PTY bytes, requests, runtime logs, binary hash) goes to
`RUSTAIN_CAPTURE_EVIDENCE_DIR`.
"""
from contextlib import ExitStack
from datetime import datetime, timezone
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import queue
import re
import signal
import subprocess
import tempfile
import threading
import time
import traceback
from urllib.parse import urlsplit

import harness
import pexpect
import pytest
from keys import BACKSPACE, CTRL_X, ENTER, ESC, SHIFT_TAB
from test_story_20_9a_ephemeral_rows import _finish, _start_text

KEY = "sk-ant-21-1-loopback-key-canary"
UNKNOWN = "Unknown command: /nope — not a built-in or a custom command."
PROFILE_REMEDY = ("Profile switching is not available in an attached session. "
                  "Run `rustain profile list` or `rustain profile show <name>` in a terminal.")
# Configured lifetime is 1 s; the defaults are 4 s (Info) and 10 s (Warning), so a
# toast gone well before them proves the `[notifications]` configuration applied.
INFO_GONE_WITHIN = 3.0
WARNING_GONE_WITHIN = 6.0
CONFIG = (
    '[permissions]\nalways_tools = ["Read", "Write", "Edit", "Bash", "Glob", "Grep"]\n'
    "\n[notifications]\ninfo_ttl_secs = 1\nwarning_ttl_secs = 1\n"
)
_TOAST = re.compile(r"(?<![A-Za-z0-9])(Info|Success|Warning|Error) (\S.*)$")
_EDGE_SLACK = 2  # a toast ends within this many columns of the 80-column pane edge
CONFIGURED_TTL = 1.0  # `info_ttl_secs` / `warning_ttl_secs` in CONFIG
_NOT_BORDERED = "┌┐└┘│─"


def _chunk(data):
    return f"{len(data):x}\r\n".encode() + data + b"\r\n"


class _NativeTUI(harness.RustainTUI):
    """Shared native spawn: PTY-driven waits only, read in small chunks so every
    intermediate frame (a one-second toast) is evaluated, never slept past."""

    def check_server(self):
        """Set by the test: raises when the loopback provider handler failed."""

    def _sync_screen(self):
        self.check_server()
        try:
            data = self.child.read_nonblocking(size=2048, timeout=0.2)
        except pexpect.TIMEOUT:
            assert self.child.isalive(), "native TUI exited unexpectedly"
            return
        except pexpect.EOF:
            pytest.fail("native TUI exited before the expected screen")
        self._stream.feed(data)
        screen = "\n".join(self._screen.display)
        for hint in self.forbidden:
            assert hint not in screen, f"retired prompt text {hint!r} painted:\n{screen}"
        if screen != self._last_recorded:
            self._last_recorded = screen
            self.screens.write(json.dumps({"screen": screen}) + "\n")
            self.screens.flush()

    _last_recorded = None
    forbidden = ()  # screen text that must never be painted (set by the S2 leg)

    def wait_for_screen(self, text, timeout=60, poll_interval=0.5):
        _wait(self, lambda screen: text in screen, repr(text), timeout)
        return True  # Never enter the shared harness's timeout-to-sleep fallback.

    def stop(self):
        if self._child is not None:
            if self.child.isalive():
                self.child.terminate(force=True)
            self.child.close(force=True)


def _wait(tui, predicate, label, timeout=30):
    deadline = time.monotonic() + timeout
    while True:
        screen = tui.get_screen_text()
        if predicate(screen):
            return screen
        assert time.monotonic() < deadline, f"Timed out waiting for {label}:\n{screen}"


def _composer(screen):
    """Text inside the bottom-most bordered box (the composer), positional."""
    lines = screen.split("\n")
    bottom = max((i for i, line in enumerate(lines) if line.strip().startswith("└")), default=None)
    top = harness.composer_top(lines)
    if bottom is None or top is None:
        return ""
    rows = [line.strip().strip("│").rstrip() for line in lines[top + 1:bottom]]
    return "\n".join(row.strip() for row in rows).strip()


def _menu(screen):
    """Text of the Ctrl+X leader box (the bordered box titled `Ctrl+X`), else ''."""
    lines = screen.split("\n")
    for i, line in enumerate(lines):
        if line.strip().startswith("┌") and "Ctrl+X" in line:
            end = next((j for j in range(i + 1, len(lines)) if lines[j].strip().startswith("└")), len(lines))
            return "\n".join(lines[i + 1:end])
    return ""


def _menu_painted(screen):
    """The whole leader box is on screen: closing border and last rail item. A
    frame reaches the screen in chunks, so a first-row match is not a painted menu."""
    lines = screen.split("\n")
    for i, line in enumerate(lines):
        if line.strip().startswith("┌") and "Ctrl+X" in line:
            return any(lines[j].strip().startswith("└") for j in range(i + 1, len(lines))) \
                and "[Esc] Cancel" in _menu(screen)
    return False


def _toasts(screen, rows=3):
    """(severity, text) of every toast row painted above the status row.

    A toast is right-aligned: it is the tail of its row and reaches the pane's
    right edge. A transcript line that merely starts with a severity word
    (`Error handling ...`) stops short of the edge and is not a toast."""
    found = []
    for row in harness.toast_rows(screen, rows):
        text = row.rstrip()
        match = _TOAST.search(text)
        if match and len(text) >= harness.TERM_COLS - _EDGE_SLACK:
            found.append((match.group(1), match.group(2)))
    return found


def _badge(screen):
    """The `Log !N` badge text in the status row (`N`, `99+`), else None."""
    match = re.search(r"(?<![A-Za-z0-9])Log !(\d+\+?)", harness.status_row(screen))
    return match.group(1) if match else None


def _capture(tui, evidence, name, screen=None):
    screen = "\n".join(tui._screen.display) if screen is None else screen
    assert tui._screen.columns == 80 and len(tui._screen.display) == 24
    (evidence / f"{name}.screen.txt").write_text(screen + "\n")
    return screen


def _expect_toast(tui, evidence, name, severity, text):
    """Wait for ONE toast `<severity> <text>` on the chat pane's bottom row (the
    row directly above the status row), then check its geometry: borderless and
    right-aligned, severity word first. Returns (screen, first-seen time)."""
    screen = _wait(tui, lambda s: harness.toast_present(s, text, severity),
                   f"the {severity} toast {text!r}", timeout=15)
    seen = time.monotonic()
    row = harness.toast_rows(screen, 1)[0].rstrip()
    match = re.search(rf"(?<![A-Za-z0-9]){severity} {re.escape(text)}(?: ×\d+)?$", row)
    assert match, row
    assert len(row) >= 78, f"toast must be right-aligned to the pane edge: {row!r}"
    assert not any(ch in row for ch in _NOT_BORDERED), f"below 100 columns a toast is borderless: {row!r}"
    assert row[match.start():].startswith(f"{severity} "), row
    _capture(tui, evidence, name, screen)
    return screen, seen


def _expect_gone(tui, seen, limit, started):
    """The toast rows empty out on their own: not before the configured 1 s
    lifetime (measured from just before the triggering key) and sooner than the
    default lifetime (measured from first sight)."""
    screen = _wait(tui, lambda s: not _toasts(s), "the toast to expire", timeout=12)
    now = time.monotonic()
    assert now - started >= CONFIGURED_TTL, \
        f"toast vanished {now - started:.2f}s after its trigger, before its configured 1 s lifetime:\n{screen}"
    elapsed = now - seen
    assert elapsed < limit, f"toast outlived its configured 1 s lifetime ({elapsed:.1f}s):\n{screen}"
    return screen


def _open_log(tui, evidence, name, attached):
    """`Ctrl+X` then `n`: the leader menu lists Notifications / Clear toasts
    after Help (and Detach, attached) and before Send / Cancel; `n` opens S-LOG."""
    tui.send(CTRL_X)
    screen = _wait(tui, lambda s: _menu_painted(s) and "[N] Notifications" in _menu(s) and "[X] Clear toasts" in _menu(s),
                   "the leader menu lists Notifications and Clear toasts, fully painted")
    menu = _menu(screen)
    order = ["[?] Help", "[N] Notifications", "[X] Clear toasts", "[Enter] Send", "[Esc] Cancel"]
    if attached:
        order.insert(0, "[D] Detach")
    positions = [menu.find(item) for item in order]
    assert all(p >= 0 for p in positions) and positions == sorted(positions), (order, positions, menu)
    tui.send("n")
    # A frame can reach the screen mid-way: the modal rows arrive before the status row
    # below them, so a complete S-LOG frame has the rail painted AND the badge cleared.
    screen = _wait(tui, lambda s: harness.NOTIFICATION_LOG_TITLE in s and not _menu(s)
                   and "[Esc] Close" in harness.notification_log_text(s) and _badge(s) is None,
                   "S-LOG (System · Notifications), complete frame, badge cleared")
    return _capture(tui, evidence, name, screen)


def _exercise(binary, evidence, root, monkeypatch):
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    errors = queue.Queue()
    requests = []
    lock = threading.Lock()
    ws_local, ws_att = root / "wl", root / "wa"
    for ws in (ws_local, ws_att):
        (ws / ".rustain").mkdir(parents=True)
        (ws / ".rustain" / "config.toml").write_text(CONFIG)
        (ws / ".env").write_text("")  # keep the shared harness from copying a developer .env
    assert len(str(root / "da" / "daemons" / ("a" * 16 + ".sock"))) <= 90
    (root / "dl").mkdir()
    (root / "da").mkdir()

    def check_server():
        try:
            error = errors.get_nowait()
        except queue.Empty:
            return
        raise AssertionError(f"loopback handler failed:\n{error}")

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_args):
            pass

        def respond(self, content, kind="application/json"):
            self.send_response(200)
            self.send_header("Content-Type", kind)
            self.send_header("Content-Length", str(len(content)))
            self.end_headers()
            self.wfile.write(content)

        def do_GET(self):
            try:
                path = urlsplit(self.path).path
                assert path in ("/api.json", "/v1/models", "/models"), path
                self.respond(b"{}" if path == "/api.json" else b'{"data":[],"has_more":false}')
            except Exception:
                errors.put(traceback.format_exc())
                self.close_connection = True

        def do_POST(self):
            try:
                assert urlsplit(self.path).path == "/v1/messages", self.path
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                if "stream" not in body and body.get("max_tokens") == 1:
                    self.respond(b'{"type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}')
                    return
                with lock:
                    requests.append({"kind": "turn", "body": body})
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Transfer-Encoding", "chunked")
                self.end_headers()
                self.wfile.write(_chunk(_start_text("S1 reply") + _finish()))
                self.wfile.write(b"0\r\n\r\n")
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                errors.put(traceback.format_exc())
            except Exception:
                errors.put(traceback.format_exc())
                self.close_connection = True

    with ExitStack() as stack:
        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        server.daemon_threads = True
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()

        def close_server():
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)
            assert not thread.is_alive()

        stack.callback(close_server)
        origin = f"http://127.0.0.1:{server.server_port}"
        loaded = harness._load_env()
        cleared = {name for name in loaded if re.search(
            r"(?:API_KEY|AUTH_TOKEN|ACCESS_TOKEN|BASE_URL|ENDPOINT|PROXY|CREDENTIALS|PROJECT|PROJECT_ID)$", name, re.I)
            or name.startswith("RUSTAIN_")
            or name in {"OLLAMA_HOST", "OPENAI_CODEX_OAUTH_TOKEN"}}

        def environment(tag, ws, data):
            env = {name: "" for name in cleared}
            env.update(HOME=str(root / "home"), XDG_CONFIG_HOME=str(root / "home" / "xdg-config"),
                       XDG_DATA_HOME=str(root / "home" / "xdg-data"), XDG_CACHE_HOME=str(root / "home" / "xdg-cache"),
                       RUSTAIN_CONFIG_DIR=str(ws / ".rustain"), RUSTAIN_DATA_DIR=str(data),
                       RUSTAIN_LOG_PATH=str(evidence / f"{tag}.runtime.log"), RUST_LOG="debug",
                       ANTHROPIC_API_KEY=KEY, ANTHROPIC_AUTH_TOKEN="", ANTHROPIC_BASE_URL=origin,
                       RUSTAIN_MODELS_DEV_URL=origin, NO_PROXY="*", no_proxy="*", TERM="xterm-256color")
            return env

        (root / "home").mkdir()
        envs = {"local": environment("local", ws_local, root / "dl"),
                "daemon": environment("daemon", ws_att, root / "da"),
                "attach": environment("attach", ws_att, root / "da")}
        terminals = {tag: stack.enter_context((evidence / f"{tag}.terminal.txt").open("w"))
                     for tag in ("local", "attach")}
        screens = {tag: stack.enter_context((evidence / f"{tag}.screens.jsonl").open("w"))
                   for tag in ("local", "attach")}
        original_spawn = harness.pexpect.spawn
        spawn_tag = {}

        def spawn(command, *args, **kwargs):
            tag = spawn_tag["next"]
            actual = kwargs["env"]
            assert Path(command).resolve() == binary and kwargs["dimensions"] == (24, 80)
            assert actual["ANTHROPIC_BASE_URL"] == origin and actual["ANTHROPIC_API_KEY"] == KEY
            kwargs["logfile"] = terminals[tag]
            return original_spawn(command, *args, **kwargs)

        monkeypatch.setattr(harness.pexpect, "spawn", spawn)

        def start(tag, ws, **kwargs):
            spawn_tag["next"] = tag
            tui = _NativeTUI(binary=binary, build=False, workspace=ws, env_overrides=envs[tag], **kwargs)
            tui.screens, tui.check_server = screens[tag], check_server
            stack.callback(tui.stop)
            tui.start()
            tui.child.delaybeforesend = 0
            return tui

        def await_exit(tui, timeout=30):
            """Read until EOF (answering a System/Quit decision with `y` once)."""
            deadline = time.monotonic() + timeout
            answered = False
            while True:
                try:
                    data = tui.child.read_nonblocking(size=65536, timeout=1)
                except pexpect.TIMEOUT:
                    assert time.monotonic() < deadline, "process did not exit"
                    continue
                except pexpect.EOF:
                    break
                tui._stream.feed(data)
                if not answered and "System · Quit" in "\n".join(tui._screen.display):
                    tui.send("y")
                    answered = True
            tui.child.close()
            return tui.child.exitstatus, tui.child.signalstatus

        # ------------------------------------------------------------ local TUI
        tui = start("local", ws_local, fresh=True)
        screen = _wait(tui, lambda s: "Ready" in harness.status_row(s) and not _toasts(s),
                       "an idle chat with no startup toast", timeout=20)
        assert re.search(r"│ Normal(?: │|\s*$)", harness.status_row(screen)), harness.status_row(screen)
        assert not tui._screen.cursor.hidden, "Input focus at start"
        baseline = int((_badge(screen) or "0").rstrip("+"))
        _capture(tui, evidence, "01-idle")

        # (a) Info: Shift+Tab cycles Normal -> AutoEdit -> Plan -> Normal; each
        # change is an Info toast `Permission mode: <Mode>` that expires by itself.
        for mode, segment in (("AutoEdit", "AUTOEDIT"), ("Plan", "PLAN"), ("Normal", "Normal")):
            started = time.monotonic()
            tui.send(SHIFT_TAB)
            screen, seen = _expect_toast(tui, evidence, f"02-info-{mode.lower()}", "Info",
                                         f"Permission mode: {mode}")
            assert re.search(rf"│ {segment}(?: │|\s*$)", harness.status_row(screen)), harness.status_row(screen)
            _expect_gone(tui, seen, INFO_GONE_WITHIN, started)

        # (a) Warning: an unknown slash command is an Advisory -> Warning toast.
        tui.send("/nope ")
        _wait(tui, lambda s: "/nope" in _composer(s), "the unknown slash typed")
        started = time.monotonic()
        tui.send(ENTER)
        _wait(tui, lambda s: "/nope" not in _composer(s), "the unknown slash consumed")
        screen, seen = _expect_toast(tui, evidence, "03-warning", "Warning", UNKNOWN)
        screen = _expect_gone(tui, seen, WARNING_GONE_WITHIN, started)
        _capture(tui, evidence, "04-expired", screen)

        # (b) The unseen-warning badge shows once the Advisory expired.
        screen = _wait(tui, lambda s: _badge(s) is not None and "Ready" in harness.status_row(s),
                       "the Log !N badge in the status row after the Warning expired", timeout=5)
        assert int(_badge(screen).rstrip("+")) >= baseline + 1, (baseline, harness.status_row(screen))
        _capture(tui, evidence, "05-badge", screen)

        # (b) Ctrl+X n: S-LOG.
        screen = _open_log(tui, evidence, "06-log", attached=False)
        lines = screen.split("\n")
        title = next(line for line in lines if harness.NOTIFICATION_LOG_TITLE in line)
        assert title.strip().startswith("┌ System · Notifications "), title
        text = harness.notification_log_text(screen)
        header = re.search(r"Notifications \((\d+)\)", text)
        assert header and int(header.group(1)) >= 4, text
        for item in ("[j/k] Move", "[Enter] Detail", "[c] Copy", "[x] Dismiss", "[Esc] Close"):
            assert item in text, (item, text)
        assert "[r] Retry" not in text, "Retry is only offered on an eligible Error row"
        rows = harness.notification_log_rows(screen)
        assert re.match(r"Warning · \d{2}:\d{2}:\d{2} · system · Unknown command: /nope", rows[0]), rows

        def row_index(mode):
            return next(i for i, row in enumerate(rows)
                        if re.match(rf"Info · \d{{2}}:\d{{2}}:\d{{2}} · \S+ · Permission mode: {mode}", row))

        assert 0 < row_index("Normal") < row_index("Plan") < row_index("AutoEdit"), rows  # newest first
        assert _badge(screen) is None and "Ready" in harness.status_row(screen), harness.status_row(screen)

        # (b) Enter: the FULL message; Esc closes and restores Input focus.
        detail = tui.read_notification_detail("Unknown command: /nope")
        assert re.search(r"Warning · \d{2}:\d{2}:\d{2} · system", detail), detail
        assert " ".join(UNKNOWN.split()) in detail, detail
        _capture(tui, evidence, "07-log-detail")
        tui.close_notification_log()
        screen = _wait(tui, lambda _s: not tui._screen.cursor.hidden, "Input focus restored after S-LOG")
        assert _badge(screen) is None, "the badge stays clear after S-LOG closed"
        assert not _toasts(screen), "closing S-LOG replays no toast"
        tui.send("\x11")
        assert await_exit(tui) == (0, None)
        check_server()

        # ------------------------------------------------------ attached client
        daemon_log = stack.enter_context((evidence / "daemon.stdout.txt").open("w"))
        daemon = subprocess.Popen([str(binary), "daemon", "start", "--foreground"], cwd=ws_att,
                                  env={**os.environ, **envs["daemon"]}, stdout=daemon_log,
                                  stderr=subprocess.STDOUT, start_new_session=True)

        def stop_daemon():
            subprocess.run([str(binary), "daemon", "stop"], cwd=ws_att, env={**os.environ, **envs["daemon"]},
                           capture_output=True, timeout=20)
            if daemon.poll() is None:
                os.killpg(daemon.pid, signal.SIGTERM)
                try:
                    daemon.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    os.killpg(daemon.pid, signal.SIGKILL)
            daemon.wait(timeout=10)

        stack.callback(stop_daemon)
        deadline = time.monotonic() + 60
        while True:
            status = subprocess.run([str(binary), "daemon", "status"], cwd=ws_att,
                                    env={**os.environ, **envs["daemon"]}, capture_output=True, text=True, timeout=10)
            if status.returncode == 0 and "running" in (status.stdout + status.stderr).lower():
                break
            try:
                daemon.wait(timeout=0.25)  # structural pacing: also notices a dead daemon
                raise AssertionError("daemon exited before becoming ready")
            except subprocess.TimeoutExpired:
                pass
            assert time.monotonic() < deadline, status.stdout + status.stderr

        tui = start("attach", ws_att, fresh=False, extra_args=["daemon", "attach"])
        screen = _wait(tui, lambda s: "attached" in harness.status_row(s) and not _toasts(s),
                       "the attached status chip with no attach-time toast", timeout=20)
        _capture(tui, evidence, "11-attached")

        # (c) An unavailable chord answers with an Info toast carrying its remedy
        # (first line only, ellipsized to the pane width), then expires.
        tui.send(CTRL_X)
        _wait(tui, lambda s: "[P] Profile (n/a)" in _menu(s), "the attached leader menu")
        started = time.monotonic()
        tui.send("p")
        lead = "Profile switching is not available in an attached session."
        screen = _wait(tui, lambda s: any(sev == "Info" and text.startswith(lead) and text.endswith("…")
                                          for sev, text in _toasts(s, 1)), "the attached Info toast")
        seen = time.monotonic()
        row = harness.toast_rows(screen, 1)[0].rstrip()
        assert len(row) >= 78 and not any(ch in row for ch in _NOT_BORDERED), row
        _capture(tui, evidence, "12-attached-toast", screen)
        _expect_gone(tui, seen, INFO_GONE_WITHIN, started)
        assert _badge(tui.get_screen_text()) is None, "an Info notice never raises the Log badge"

        screen = _open_log(tui, evidence, "13-attached-log", attached=True)
        text = harness.notification_log_text(screen)
        assert re.search(r"Notifications \((\d+)\)", text), text
        rows = harness.notification_log_rows(screen)
        # The list row is one ellipsized line at 80 columns; the FULL remedy is the
        # detail assertion below.
        assert re.match(r"Info · \d{2}:\d{2}:\d{2} · chord · Profile switching is not available in an attached",
                        rows[0]), rows
        detail = tui.read_notification_detail("Profile switching is not available")
        assert " ".join(PROFILE_REMEDY.split()) in detail, detail
        _capture(tui, evidence, "14-attached-log-detail")
        tui.close_notification_log()
        screen = _wait(tui, lambda s: "attached" in harness.status_row(s) and not tui._screen.cursor.hidden,
                       "still attached with Input focus after S-LOG closed")
        assert _badge(screen) is None and not _toasts(screen), screen

        # Ctrl+X d detaches this client only; the daemon keeps running.
        tui.send(CTRL_X)
        _wait(tui, lambda s: "[D] Detach" in _menu(s), "the leader menu before d")
        tui.send("d")
        assert await_exit(tui) == (0, None)

        # ------------------------------------------------------ evidence checks
        with lock:
            observed = list(requests)
        (evidence / "requests.json").write_text(json.dumps(observed, indent=2) + "\n")
        assert observed == [], "no provider turn: every notice here is a local producer"
        check_server()
        for handle in (*terminals.values(), *screens.values(), daemon_log):
            handle.flush()
        logs = sorted(evidence.glob("*.runtime.log.*"))
        for tag in ("local", "daemon", "attach"):
            assert any(path.name.startswith(f"{tag}.runtime.log.") and re.fullmatch(r"\d{4}-\d{2}-\d{2}", path.suffix[1:])
                       for path in logs), f"dated {tag} debug log missing"
        for path in [*(evidence / f"{tag}.terminal.txt" for tag in terminals),
                     *(evidence / f"{tag}.screens.jsonl" for tag in screens),
                     evidence / "daemon.stdout.txt", evidence / "requests.json", *logs,
                     *evidence.glob("*.screen.txt")]:
            assert KEY not in path.read_text(errors="replace"), f"credential leaked to {path}"


def _fixture_screen(*chat_rows):
    """A composer-anchored 80-column screen whose chat pane ends with `chat_rows`."""
    box = ["┌" + "─" * 78 + "┐", "│" + " " * 78 + "│", "└" + "─" * 78 + "┘"]
    return "\n".join([*chat_rows, "status row │ Normal │ Ready", *box])


def test_toast_helpers_ignore_transcript_text_that_only_looks_like_a_toast(monkeypatch):
    """A transcript line `Error handling ...` is not a toast, and a transcript
    mention of the permission-mode text is not the permission-mode toast."""
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    transcript = "Error handling is described in the docs"
    assert _toasts(_fixture_screen("older", transcript)) == []
    real = "Error provider failed".rjust(80)
    assert _toasts(_fixture_screen("older", real)) == [("Error", "provider failed")]
    assert _toasts(_fixture_screen(transcript, real)) == [("Error", "provider failed")]
    screen = _fixture_screen("I read Permission mode: Plan in the docs", "older")
    assert not harness.toast_present(screen, "Permission mode: Plan", "Info", rows=9)
    toast = "Info Permission mode: Plan".rjust(80)
    assert harness.toast_present(_fixture_screen("older", toast), "Permission mode: Plan", "Info", rows=9)


def test_status_row_untouched_predicate_tracks_the_documented_clip_only():
    """The CI row (`ctx:` known, state word clipped by the reserved cue) and the
    local row (no `ctx:`, `Ready` visible) are both untouched; a row that lost
    its mode segment, its cue, or its state word without a `ctx:` segment is not."""
    ci = " anthropic/claude-sonnet-4-6 [F] │ Normal │ ctx: 0/1m (0%) │  Needs you (1) ^X q"
    local = " anthropic/claude-sonnet-4-6 [F] │ Normal │ Ready │ █ coding  Needs you (1) ^X q"
    assert "Ready" not in ci, "the CI row has no state word: the old predicate failed here"
    assert _status_row_untouched(ci) and _status_row_untouched(local)
    assert not _status_row_untouched(" anthropic/claude-sonnet-4-6 [F] │ Normal │  Needs you (1) ^X q")
    assert not _status_row_untouched(" anthropic/claude-sonnet-4-6 [F] │ Ready │ █ coding  Needs you (1) ^X q")
    assert not _status_row_untouched(" anthropic/claude-sonnet-4-6 [F] │ Normal │ Ready │ █ coding")


@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(300)
def test_s1_toasts_expire_and_notification_log_local_and_attached_80x24(monkeypatch):
    binary = harness._resolve_binary()[0].resolve()
    assert binary.is_file() and "debug" in binary.parts and "release" not in binary.parts, binary
    parent = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())).resolve()
    parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="21-1-s1-", dir=parent))
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    manifest = {"binary": str(binary), "sha256": digest, "bytes": binary.stat().st_size, "viewport": [80, 24],
                "started_at": datetime.now(timezone.utc).isoformat(), "result": "RUNNING"}
    try:
        # Short root: the daemon socket path must fit sockaddr_un.
        with tempfile.TemporaryDirectory(prefix="r211-", dir="/tmp") as root:
            _exercise(binary, evidence, Path(root), monkeypatch)
        assert hashlib.sha256(binary.read_bytes()).hexdigest() == digest
        manifest["result"] = "PASS"
    except BaseException:
        manifest["result"] = "FAIL"
        (evidence / "failure.txt").write_text(traceback.format_exc())
        raise
    finally:
        manifest["completed_at"] = datetime.now(timezone.utc).isoformat()
        (evidence / "run.json").write_text(json.dumps(manifest, indent=2) + "\n")
        print(f"NATIVE_21_1_S1_EVIDENCE={evidence}", flush=True)


# ═══════════════════════════════════════════════════════════════════════════
# S2 leg (K12b): the interim decision pane through the real binary at 80x24.
#
# * recovery - a first TUI run completes one turn and is `kill -9`ed; a restart
#   on the same data directory paints the recovery pane
#   (`[Enter/Esc] Continue  [n] New session`); `Esc` (retried structurally: rail
#   keys are inert for 500 ms after the first paint and are never replayed)
#   continues the SAME conversation and the composer is not swallowed.
# * budget - a tiny `[budget] daily_limit_usd` crosses its limit when a turn
#   completes while a draft sits in the composer: the decision is NOT painted
#   over the draft (the draft is intact and keeps taking keys), the status row
#   shows `Needs you (1) ^X q`, `Ctrl+X q` opens the list, Enter opens the
#   selected pane, `Esc` defers it (the cue stays), and `y` resolves it.
# * the retired prompt texts (`[y/n]`, `Ctrl+K`) are never painted.
# Every wait is a bounded structural predicate; no sleeps.
# ═══════════════════════════════════════════════════════════════════════════

S2_CONFIG = CONFIG
BUDGET_CONFIG = (
    S2_CONFIG
    + '\n[budget]\ndaily_limit_usd = 0.000001\n'
    + '\n[pricing."claude-sonnet-4-6"]\ninput_per_million = 3.0\noutput_per_million = 15.0\n'
)
RETIRED_HINTS = ("[y/n]", "Ctrl+K")
WAITING_TOAST = "Decision waiting — Ctrl+X q"
_CUE = re.compile(r"Needs you \((\d+)\) \^X q")


def _pane_lines(screen, title):
    """The frame titled `Decision: <title>` (top border row index and the lines), else None."""
    lines = screen.split("\n")
    marker = f"Decision: {title} "
    top = next((i for i, line in enumerate(lines) if "┌" in line and marker in line), None)
    return None if top is None else (top, lines)


def _pane(screen, title):
    """Whitespace-normalised interior text of the decision frame, '' while it is
    only partly drawn, None when no such frame is painted."""
    found = _pane_lines(screen, title)
    if found is None:
        return None
    top, lines = found
    left, right = lines[top].index("┌"), lines[top].rfind("┐")
    bottom = next((i for i in range(top + 1, len(lines)) if len(lines[i]) > left and lines[i][left] == "└"), None)
    if bottom is None or right <= left:
        return ""
    return " ".join(" ".join(line[left + 1:right] for line in lines[top + 1:bottom]).split())


def _pane_open(screen, title, *rail):
    """The frame is fully painted with every rail pair (positional: inside the frame)."""
    text = _pane(screen, title)
    return bool(text) and all(item in text for item in rail)


def _open_budget_from_list(tui, rail):
    """Open the selected budget from a fully painted Needs-you list."""
    def listed(screen):
        rows = screen.splitlines()
        top = next((i for i, row in enumerate(rows) if row.startswith("┌ Needs you ")), None)
        if top is None:
            return False
        bottom = next((i for i in range(top + 1, len(rows))
                       if rows[i].startswith("└") and rows[i].endswith("┘")), None)
        if bottom is None or any(not (row.startswith("│") and row.endswith("│"))
                                 for row in rows[top + 1:bottom]):
            return False
        body = rows[top + 1:bottom]
        return (any("▸ Daily budget ·" in row for row in body)
                and any("[Enter] Open" in row and "[Esc] Back" in row
                        and "[Up/Down] Select" in row for row in body))

    _wait(tui, listed, "the Needs-you list with the selected budget and its rail")
    deadline = time.monotonic() + 0.6
    while time.monotonic() < deadline:
        screen = tui.get_screen_text()
        assert listed(screen), f"the Needs-you list changed before Enter:\n{screen}"
    tui.send(ENTER)
    return _wait(tui, lambda s: _pane_open(s, "Daily budget", *rail),
                 "Enter opens the budget decision with its rail")


def _cue(screen):
    """The `Needs you (N) ^X q` status cue count, else None (status row only)."""
    match = _CUE.search(harness.status_row(screen))
    return int(match.group(1)) if match else None


def _status_row_untouched(row):
    """The pane leaves the status row's own content in place: the mode segment
    survives and the `Needs you` cue is reserved at the right edge. The state
    word (`Ready`) is the LAST left segment, so at 80 columns it clips exactly
    when the model/`ctx:` segments plus the reserved cue fill the row. AC10 and
    `status_bar.rs` accept that (the cue clips the left content, never itself);
    the clip is the registered DF-21-1-STATUS-ROW-STATE-WORD-CLIPPED, owner 21-4.
    Whether `ctx:` is known at that instant is timing, so the state word may be
    absent only while the `ctx:` segment explains it."""
    left = row.split("Needs you")[0]
    return (_CUE.search(row) is not None and "│ Normal" in left
            and ("Ready" in left or "ctx:" in left))


def _clear_composer(tui, label):
    """Backspace the single-line draft away (Ctrl+U is not bound in Input focus)."""
    text = _composer(tui.get_screen_text())
    tui.send(BACKSPACE * len(text))
    _wait(tui, lambda s: _composer(s) == "", label)


def _answer(tui, key, title, tries=8):
    """Press `key` until the pane titled `title` is gone. Rail keys are inert for
    500 ms after the first paint and are never replayed, so a key pressed inside
    that window is dropped: re-press, one press per observed second of frames
    (structural, no sleep), until the frame disappears."""
    for _ in range(tries):
        tui.send(key)
        deadline = time.monotonic() + 1.0
        while time.monotonic() < deadline:
            screen = tui.get_screen_text()
            if _pane_lines(screen, title) is None:
                return screen
    raise AssertionError(f"the {title!r} pane never accepted {key!r}:\n{tui.get_screen_text()}")


def _await_exit(tui, timeout=30):
    """Read until EOF (answering a System/Quit decision with `y` once)."""
    deadline = time.monotonic() + timeout
    answered = False
    while True:
        try:
            data = tui.child.read_nonblocking(size=65536, timeout=1)
        except pexpect.TIMEOUT:
            assert time.monotonic() < deadline, "process did not exit"
            continue
        except pexpect.EOF:
            break
        tui._stream.feed(data)
        if not answered and "System · Quit" in "\n".join(tui._screen.display):
            tui.send("y")
            answered = True
    tui.child.close()
    return tui.child.exitstatus, tui.child.signalstatus


def _exercise_s2(binary, evidence, root, monkeypatch):
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    errors = queue.Queue()
    requests = []
    holds = {}  # marker -> Event: a turn whose user text contains the marker waits for the event
    lock = threading.Lock()
    workspaces = {"r": root / "wr", "b": root / "wb"}
    for name, ws in workspaces.items():
        (ws / ".rustain").mkdir(parents=True)
        (ws / ".rustain" / "config.toml").write_text(BUDGET_CONFIG if name == "b" else S2_CONFIG)
        (ws / ".env").write_text("")
    (root / "home").mkdir()

    def check_server():
        try:
            error = errors.get_nowait()
        except queue.Empty:
            return
        raise AssertionError(f"loopback handler failed:\n{error}")

    def user_text(body):
        for message in reversed(body.get("messages", [])):
            if message.get("role") == "user":
                content = message.get("content")
                if isinstance(content, str):
                    return content
                return " ".join(block.get("text", "") for block in content if isinstance(block, dict))
        return ""

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_args):
            pass

        def respond(self, content, kind="application/json"):
            self.send_response(200)
            self.send_header("Content-Type", kind)
            self.send_header("Content-Length", str(len(content)))
            self.end_headers()
            self.wfile.write(content)

        def do_GET(self):
            try:
                path = urlsplit(self.path).path
                assert path in ("/api.json", "/v1/models", "/models"), path
                self.respond(b"{}" if path == "/api.json" else b'{"data":[],"has_more":false}')
            except Exception:
                errors.put(traceback.format_exc())
                self.close_connection = True

        def do_POST(self):
            try:
                assert urlsplit(self.path).path == "/v1/messages", self.path
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                if "stream" not in body and body.get("max_tokens") == 1:
                    self.respond(b'{"type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}')
                    return
                text = user_text(body)
                with lock:
                    requests.append({"kind": "turn", "text": text})
                for marker, event in list(holds.items()):
                    if marker in text:
                        assert event.wait(90), f"held reply {marker!r} was never released"
                first = text.split()[0] if text.split() else "?"
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Transfer-Encoding", "chunked")
                self.end_headers()
                self.wfile.write(_chunk(_start_text(f"S2 reply {first}") + _finish()))
                self.wfile.write(b"0\r\n\r\n")
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                errors.put(traceback.format_exc())
            except Exception:
                errors.put(traceback.format_exc())
                self.close_connection = True

    with ExitStack() as stack:
        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        server.daemon_threads = True
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()

        def close_server():
            for event in holds.values():
                event.set()
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)
            assert not thread.is_alive()

        stack.callback(close_server)
        origin = f"http://127.0.0.1:{server.server_port}"
        loaded = harness._load_env()
        cleared = {name for name in loaded if re.search(
            r"(?:API_KEY|AUTH_TOKEN|ACCESS_TOKEN|BASE_URL|ENDPOINT|PROXY|CREDENTIALS|PROJECT|PROJECT_ID)$", name, re.I)
            or name.startswith("RUSTAIN_")
            or name in {"OLLAMA_HOST", "OPENAI_CODEX_OAUTH_TOKEN"}}

        def environment(tag, ws, data):
            env = {name: "" for name in cleared}
            env.update(HOME=str(root / "home"), XDG_CONFIG_HOME=str(root / "home" / "xdg-config"),
                       XDG_DATA_HOME=str(root / "home" / "xdg-data"), XDG_CACHE_HOME=str(root / "home" / "xdg-cache"),
                       RUSTAIN_CONFIG_DIR=str(ws / ".rustain"), RUSTAIN_DATA_DIR=str(data),
                       RUSTAIN_LOG_PATH=str(evidence / f"{tag}.runtime.log"), RUST_LOG="debug",
                       ANTHROPIC_API_KEY=KEY, ANTHROPIC_AUTH_TOKEN="", ANTHROPIC_BASE_URL=origin,
                       RUSTAIN_MODELS_DEV_URL=origin, NO_PROXY="*", no_proxy="*", TERM="xterm-256color")
            return env

        data_r, data_b = root / "dr", root / "db"
        data_r.mkdir()
        data_b.mkdir()
        tags = {"r1": ("r", data_r), "r2": ("r", data_r), "b": ("b", data_b)}
        envs = {tag: environment(tag, workspaces[ws], data) for tag, (ws, data) in tags.items()}
        terminals = {tag: stack.enter_context((evidence / f"{tag}.terminal.txt").open("w")) for tag in tags}
        screens = {tag: stack.enter_context((evidence / f"{tag}.screens.jsonl").open("w")) for tag in tags}
        original_spawn = harness.pexpect.spawn
        spawn_tag = {}

        def spawn(command, *args, **kwargs):
            tag = spawn_tag["next"]
            actual = kwargs["env"]
            assert Path(command).resolve() == binary and kwargs["dimensions"] == (24, 80)
            assert actual["ANTHROPIC_BASE_URL"] == origin and actual["ANTHROPIC_API_KEY"] == KEY
            kwargs["logfile"] = terminals[tag]
            return original_spawn(command, *args, **kwargs)

        monkeypatch.setattr(harness.pexpect, "spawn", spawn)

        def start(tag, **kwargs):
            spawn_tag["next"] = tag
            tui = _NativeTUI(binary=binary, build=False, workspace=workspaces[tags[tag][0]],
                             env_overrides=envs[tag], **kwargs)
            tui.screens, tui.check_server, tui.forbidden = screens[tag], check_server, RETIRED_HINTS
            stack.callback(tui.stop)
            tui.start()
            tui.child.delaybeforesend = 0
            return tui

        def sent_to_provider():
            with lock:
                return [request["text"] for request in requests]

        # ------------------------------------------------ recovery: crash + restart
        tui = start("r1", fresh=True)
        _wait(tui, lambda s: "Ready" in harness.status_row(s) and not _toasts(s) and _cue(s) is None,
              "an idle first run", timeout=20)
        tui.send("crash seed")
        _wait(tui, lambda s: "crash seed" in _composer(s), "the seed message typed")
        tui.send(ENTER)
        _wait(tui, lambda s: "S2 reply crash" in s and "Ready" in harness.status_row(s), "the first reply")

        sessions = workspaces["r"] / ".claude" / "sessions"  # `paths::sessions_dir(workspace)`

        def persisted(_screen):
            return any(b"S2 reply crash" in path.read_bytes() for path in sessions.rglob("*.json"))

        _wait(tui, persisted, "the conversation on disk before the crash", timeout=20)
        os.kill(tui.child.pid, signal.SIGKILL)  # an unclean exit: no clean-exit marker is written
        tui.child.close(force=True)
        assert not tui.child.isalive()

        tui = start("r2", fresh=False)
        rail = ("[Enter/Esc] Continue", "[n] New session")
        # The status row is below the pane and paints after it: wait for the cue (the last
        # thing on the row) so the row is read whole, not between two chunks of one frame.
        screen = _wait(tui, lambda s: _pane_open(s, "Session recovery", *rail) and _cue(s) == 1,
                       "the recovery pane with its Continue/New session rail and the status cue", timeout=20)
        _capture(tui, evidence, "21-recovery-pane", screen)
        assert "[y]" not in _pane(screen, "Session recovery"), "recovery paints no `y` key"
        assert _status_row_untouched(harness.status_row(screen)), (
            "the status row is untouched by the pane: " + harness.status_row(screen))
        screen = _answer(tui, ESC, "Session recovery")  # Esc = Continue (RU-5), never New session
        screen = _wait(tui, lambda s: "S2 reply crash" in s and "crash seed" in s,
                       "the SAME conversation continues after Esc")
        assert _cue(screen) is None, "an answered decision leaves no cue"
        _capture(tui, evidence, "22-recovery-continued", screen)
        tui.send("ok")  # the composer is live again: the pane no longer swallows keys
        _wait(tui, lambda s: _composer(s) == "ok", "typing reaches the composer after Continue")
        _clear_composer(tui, "the composer cleared")

        # Owner-screenshot storm tail: a Warning toast still expires on its own TTL.
        tui.send("/nope ")
        _wait(tui, lambda s: "/nope" in _composer(s), "the unknown slash typed")
        started = time.monotonic()
        tui.send(ENTER)
        _wait(tui, lambda s: "/nope" not in _composer(s), "the unknown slash consumed")
        screen, seen = _expect_toast(tui, evidence, "23-storm-warning", "Warning", UNKNOWN)
        screen = _expect_gone(tui, seen, WARNING_GONE_WITHIN, started)
        assert _cue(screen) is None and _pane(screen, "Session recovery") is None
        tui.send("\x11")
        assert _await_exit(tui) == (0, None)
        check_server()

        # ------------------------------------------------ budget: arrival while typing
        tui = start("b", fresh=True)
        _wait(tui, lambda s: "Ready" in harness.status_row(s) and not _toasts(s) and _cue(s) is None,
              "an idle budget run", timeout=20)

        def turn(number):
            marker = f"hold-{number}"
            holds[marker] = hold = threading.Event()
            tui.send(f"{marker} please answer")
            _wait(tui, lambda s: marker in _composer(s), f"{marker} typed")
            tui.send(ENTER)
            _wait(tui, lambda s: _composer(s) == "", f"{marker} submitted")
            _wait(tui, lambda _s: any(marker in text for text in sent_to_provider()),
                  f"{marker} to reach the provider (held)")
            draft = f"draft {number} keep"
            tui.send(draft)  # typed while the reply is held
            _wait(tui, lambda s: _composer(s) == draft, "the draft typed during the held turn")
            hold.set()
            _wait(tui, lambda s: f"S2 reply {marker}" in s,
                  f"the {marker} reply")
            return draft

        def cue_within(seconds):
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                screen = tui.get_screen_text()
                if _cue(screen):
                    return screen
            return None

        draft = turn(1)
        # The ledger write may land one turn late (event_loop: "1-turn lag at the threshold
        # crossing"), so the limit can be crossed by turn 1's or by turn 2's completion.
        screen = cue_within(3)
        if screen is None:
            _clear_composer(tui, "the first draft cleared")
            draft = turn(2)
            screen = cue_within(20)
        assert screen is not None, "the budget decision never arrived:\n" + tui.get_screen_text()
        assert _cue(screen) == 1, harness.status_row(screen)
        assert _pane(screen, "Daily budget") is None, "an arrival never paints over a draft"
        assert _composer(screen) == draft, f"the draft was hijacked: {_composer(screen)!r}"
        _capture(tui, evidence, "31-budget-waiting", screen)
        tui.send("yn")  # the rail letters are the composer's text while the decision is hidden (EV-3)
        screen = _wait(tui, lambda s: _composer(s) == draft + "yn", "typing continues in the composer")
        assert _pane(screen, "Daily budget") is None
        assert _cue(screen) == 1, "typed rail letters answered nothing: " + harness.status_row(screen)
        _clear_composer(tui, "the draft cleared")

        tui.send(CTRL_X)
        _wait(tui, lambda s: "[Q] Decisions" in _menu(s), "the leader menu lists Decisions")
        tui.send("q")
        budget_rail = ("[y] Continue", "[s] Switch model", "[p] Pause until tomorrow", "[Esc] Later")
        screen = _open_budget_from_list(tui, budget_rail)
        assert "reached" in _pane(screen, "Daily budget"), _pane(screen, "Daily budget")
        _capture(tui, evidence, "32-budget-pane", screen)
        screen = _answer(tui, ESC, "Daily budget")  # Esc = Later: defers, never resolves
        screen = _wait(tui, lambda s: _cue(s) == 1, "a deferred decision keeps its status cue")
        assert _composer(screen) == "", "the deferred decision left the composer alone"
        _capture(tui, evidence, "33-budget-deferred", screen)
        tui.send("y")  # a deferred decision is still hidden: `y` is text, never Continue
        screen = _wait(tui, lambda s: _composer(s) == "y", "typing reaches the composer after Later")
        assert _cue(screen) == 1, "Later kept the decision pending: " + harness.status_row(screen)
        _clear_composer(tui, "the probe key cleared")

        tui.send(CTRL_X)
        _wait(tui, lambda s: "[Q] Decisions" in _menu(s), "the leader menu before the second q")
        tui.send("q")
        _open_budget_from_list(tui, budget_rail)
        screen = _answer(tui, "y", "Daily budget")  # Continue: the effect runs once
        screen = _wait(tui, lambda s: _cue(s) is None, "an answered decision clears the cue")
        _capture(tui, evidence, "34-budget-answered", screen)

        screen = _open_log(tui, evidence, "35-budget-log", attached=False)
        rows = harness.notification_log_rows(screen)
        pattern = rf"Info · \d{{2}}:\d{{2}}:\d{{2}} · \S+ · {re.escape(WAITING_TOAST)}"
        assert any(re.match(pattern, row) for row in rows), rows
        tui.close_notification_log()
        _wait(tui, lambda s: not tui._screen.cursor.hidden, "Input focus restored after S-LOG")
        tui.send("\x11")
        assert _await_exit(tui) == (0, None)
        check_server()

        # ------------------------------------------------ evidence checks
        observed = sent_to_provider()
        (evidence / "requests.json").write_text(json.dumps(observed, indent=2) + "\n")
        assert any("crash seed" in text for text in observed) and any("hold-1" in text for text in observed)
        for handle in (*terminals.values(), *screens.values()):
            handle.flush()
        logs = sorted(evidence.glob("*.runtime.log.*"))
        for tag in tags:
            assert any(path.name.startswith(f"{tag}.runtime.log.") and re.fullmatch(r"\d{4}-\d{2}-\d{2}", path.suffix[1:])
                       for path in logs), f"dated {tag} debug log missing"
        for path in [*(evidence / f"{tag}.terminal.txt" for tag in tags),
                     *(evidence / f"{tag}.screens.jsonl" for tag in tags),
                     evidence / "requests.json", *logs, *evidence.glob("*.screen.txt")]:
            assert KEY not in path.read_text(errors="replace"), f"credential leaked to {path}"


@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(400)
def test_s2_decision_pane_recovery_and_arrival_protection_80x24(monkeypatch):
    binary = harness._resolve_binary()[0].resolve()
    assert binary.is_file() and "debug" in binary.parts and "release" not in binary.parts, binary
    parent = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())).resolve()
    parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="21-1-s2-", dir=parent))
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    manifest = {"binary": str(binary), "sha256": digest, "bytes": binary.stat().st_size, "viewport": [80, 24],
                "started_at": datetime.now(timezone.utc).isoformat(), "result": "RUNNING"}
    try:
        with tempfile.TemporaryDirectory(prefix="r212-", dir="/tmp") as root:
            _exercise_s2(binary, evidence, Path(root), monkeypatch)
        assert hashlib.sha256(binary.read_bytes()).hexdigest() == digest
        manifest["result"] = "PASS"
    except BaseException:
        manifest["result"] = "FAIL"
        (evidence / "failure.txt").write_text(traceback.format_exc())
        raise
    finally:
        manifest["completed_at"] = datetime.now(timezone.utc).isoformat()
        (evidence / "run.json").write_text(json.dumps(manifest, indent=2) + "\n")
        print(f"NATIVE_21_1_S2_EVIDENCE={evidence}", flush=True)
