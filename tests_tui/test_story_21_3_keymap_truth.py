"""Story 21.3 leg D: the permission card natively, attached at 80x24.

One test (`test_keymap_truth_attached_permission_card_...`, 3-C patch round, AC19, FR-21-3-c) drives the
permission natively: a daemon plus a rich attached terminal at 80x24 and a scripted loopback provider that streams a
`Bash` tool call. `x` typed before the arrival is composer text; the approval arrives with an empty composer and focus
moves to the Permission decision frame; `Esc` and `Ctrl+C` each defer it without deciding;
`Ctrl+X q` opens the Needs-you list and Enter reopens the selected permission;
`n` denies once (one tool result reaches
the provider, the command never ran).

`_PermissionTUI` and `_permission_daemon` are shared with `test_story_21_6_decision_queue.py`.

Evidence (screens, raw PTY bytes sent, requests, runtime logs) goes to `RUSTAIN_CAPTURE_EVIDENCE_DIR`.
Every wait is a bounded screen condition.
"""
import json
import os
from pathlib import Path
import re
import tempfile
import shlex
import shutil
import signal
import socket
import subprocess
import time

import harness
import pexpect
import pytest
from test_story_21_2_shortcut_truth import _NativeTUI, _composer
from test_story_19_20c_exact_approval import Evidence as _Evidence
from test_story_19_20c_exact_approval import _command, _finished, _world


def _permission_wait(tui, predicate, label, timeout=60):
    """Leg D waits on PTY frames, never the approval helper's polling sleep."""
    deadline = time.monotonic() + timeout
    while True:
        screen = tui.get_screen_text()
        if predicate(screen):
            return screen
        assert time.monotonic() < deadline, f"Timed out waiting for {label}:\n{screen}"


class _PermissionTUI(_NativeTUI):
    """Attached-only spawn into `_world`'s prepared workspace; no startup fallback sleep."""

    def start(self):
        self._binary = harness._resolve_binary()[0].resolve()
        assert self._binary.is_file() and "debug" in self._binary.parts and "release" not in self._binary.parts
        self._workspace_path = self.workspace
        env = {**harness._load_env(), **self.env_overrides}
        env.setdefault("RUST_LOG", "debug")
        self._screen = harness.TerminalScreen(harness.TERM_COLS, harness.TERM_ROWS)
        self._stream = harness.pyte.Stream(self._screen)
        self._child = pexpect.spawn(str(self._binary), args=["daemon", "attach"], cwd=str(self.workspace),
                                    env=env, timeout=self.timeout, encoding="utf-8",
                                    dimensions=(harness.TERM_ROWS, harness.TERM_COLS))
        self.child.delaybeforesend = 0
        _permission_wait(self, lambda s: "Ready" in s, "attached terminal ready")
        return self

    def _sync_screen(self):
        try:
            data = self.child.read_nonblocking(size=65536, timeout=0.2)
        except pexpect.TIMEOUT:
            assert self.child.isalive(), "attached TUI exited unexpectedly"
            return
        except pexpect.EOF:
            pytest.fail("attached TUI exited before the expected screen")
        self._stream.feed(data)

    def send(self, data):
        os.write(self.child.child_fd, data.encode("utf-8"))


def _permission_daemon(stack, ws, env, tmp_path):
    """Bound readiness by a real daemon socket accepting and sending its challenge."""
    binary = harness._resolve_binary()[0].resolve()
    assert binary.is_file() and "debug" in binary.parts and "release" not in binary.parts
    log = stack.enter_context((tmp_path / "daemon.log").open("w"))
    proc = subprocess.Popen([str(binary), "daemon", "start", "--foreground"], cwd=ws, env=env,
                            stdout=log, stderr=subprocess.STDOUT, start_new_session=True)

    def stop():
        if proc.poll() is None:
            os.killpg(proc.pid, signal.SIGTERM)
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(proc.pid, signal.SIGKILL)
        proc.wait(timeout=5)

    stack.callback(stop)
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        assert proc.poll() is None, (tmp_path / "daemon.log").read_text()
        for path in (Path(env["RUSTAIN_DATA_DIR"]) / "daemons").glob("*.sock"):
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                break
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as probe:
                probe.settimeout(min(0.2, remaining))
                try:
                    probe.connect(str(path))
                    if probe.recv(1):
                        return
                except (FileNotFoundError, ConnectionRefusedError, socket.timeout):
                    pass
    pytest.fail("Daemon socket did not become ready:\n" + (tmp_path / "daemon.log").read_text())


def test_keymap_truth_attached_permission_card_takes_chat_focus_80x24(tmp_path, monkeypatch):
    """Leg D (3-C patch round, FR-21-3-c, AC19): the permission card natively, attached at 80x24."""
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    canary = tmp_path / "canary.txt"
    invocation = {"command": "printf leg-d >> " + shlex.quote(str(canary))}
    evidence = _Evidence("21-3-leg-d")
    sent = []
    with _world(tmp_path, "Bash", invocation, evidence) as (stack, ws, stub, env):
        assert env["ANTHROPIC_API_KEY"] == "operator" and env["ANTHROPIC_BASE_URL"].startswith("http://127.0.0.1:")
        # A daemon socket path must stay under SUN_LEN: the pytest tmp tree is too long on a deep TMPDIR.
        data = Path(tempfile.mkdtemp(prefix="r213d-"))
        stack.callback(shutil.rmtree, data, ignore_errors=True)
        env["RUSTAIN_DATA_DIR"] = str(data)
        _permission_daemon(stack, ws, env, tmp_path)
        tui = _PermissionTUI(build=False, fresh=False, workspace=ws, allowed_tools=[], env_overrides=env)
        stack.callback(tui.stop)
        tui.start()
        evidence.terminal(stack, tui, "writer.terminal.txt")

        def wait(predicate, what):
            return _permission_wait(tui, predicate, what)

        # `x` typed before any card is text in the composer (Input focus), then erased.
        sent.append("x")
        tui.send("x")
        wait(lambda s: _composer(s) == "x" and not tui._screen.cursor.hidden, "`x` is composer text in Input focus")
        sent.append("Backspace")
        tui.send("\x7f")
        wait(lambda s: _composer(s) == "", "composer empty again")
        # The approval arrives with an empty composer and opens a decision frame.
        def permission_frame(s):
            rows = s.splitlines()
            return (len(rows) == 24 and rows[0].startswith("┌ Decision: Permission ")
                    and rows[18].startswith("└") and rows[18].endswith("┘")
                    and rows[19].rstrip().startswith("Decisions ")
                    and harness.strip_row(s) == rows[19].rstrip()
                    and all(row.startswith("│") and row.endswith("│") for row in rows[1:18])
                    and all(rail in s for rail in ("[y] Allow", "[s] Session", "[a] Always",
                                                   "[n] Deny", "[f] Deny + feedback", "[r] Review")))

        def settle(predicate, label):
            wait(predicate, label)
            deadline = time.monotonic() + 0.6
            while time.monotonic() < deadline:
                screen = tui.get_screen_text()
                assert predicate(screen), f"{label} changed:\n{screen}"

        def reopen():
            sent.append("Ctrl+X q")
            tui.send("\x18q")
            def listed(s):
                rows = s.splitlines()
                return (len(rows) == 24 and rows[0].startswith("┌ Needs you ")
                        and rows[18].startswith("└") and rows[18].endswith("┘")
                        and rows[19].rstrip().startswith("Decisions ")
                        and harness.strip_row(s) == rows[19].rstrip()
                        and all(row.startswith("│") and row.endswith("│") for row in rows[1:18])
                        and any("▸ Permission · Bash" in row for row in rows)
                        and "[Enter] Open  [Esc] Back  [Up/Down] Select" in s)
            settle(listed, "complete Needs-you list")
            sent.append("Enter")
            tui.send("\r")
            settle(permission_frame, "reopened permission frame beyond its guard")

        _command(tui)
        screen = wait(permission_frame, "the complete approval frame")
        evidence.text("d-01-card.screen.txt", screen)
        assert tui._screen.cursor.hidden, f"the arrival took Chat focus:\n{screen}"
        assert not canary.exists()
        # Esc defers; the status chip remains and no result reaches the provider.
        sent.append("Esc")
        tui.send("\x1b")
        screen = wait(lambda s: "Decision: Permission" not in s
                      and "Needs you (1) ^X q" in harness.status_row(s), "Esc defers the permission")
        evidence.text("d-02-esc-cue.screen.txt", screen)
        assert not canary.exists() and not _finished(stub)
        reopen()
        # Ctrl+C also defers without interrupting or quitting.
        sent.append("Ctrl+C")
        tui.send("\x03")
        screen = wait(lambda s: "Decision: Permission" not in s
                      and "Needs you (1) ^X q" in harness.status_row(s), "Ctrl+C defers the permission")
        assert tui.child.isalive() and "[interrupted]" not in screen and "System · Quit" not in screen, screen
        assert not _finished(stub), "no decision was sent"
        reopen()
        sent.append("n")
        tui.send("n")
        wait(lambda s: _finished(stub), "the provider receives the denial")
        screen = wait(lambda s: "Decision: Permission" not in s, "the frame is gone")
        evidence.text("d-03-denied.screen.txt", screen)
        assert not canary.exists(), "denial must execute nothing"
        results = [row for row in stub.rows() if row.get("turn") == 2 and row.get("tool_results") == 1]
        assert len(results) == 1, f"the provider saw exactly one decision: {stub.rows()}"
        evidence.text("sent.json", json.dumps(sent, indent=2))
    for log in sorted(tmp_path.glob("runtime.log.*")) + [tmp_path / "daemon.log"]:
        if log.is_file():
            evidence.text(log.name, log.read_text(errors="replace"))
    if evidence.root:
        assert any(p.name.startswith("runtime.log.") for p in evidence.root.iterdir()), "dated runtime log missing"
        # The persona name is the loopback "key"; a leak means a real developer credential.
        secrets = [value for name, value in harness._load_env().items()
                   if len(value) >= 16 and re.search(r"KEY|TOKEN|SECRET", name, re.I)]
        for path in evidence.root.iterdir():
            text = path.read_text(errors="replace")
            assert not any(secret in text for secret in secrets), f"credential leaked to {path}"
