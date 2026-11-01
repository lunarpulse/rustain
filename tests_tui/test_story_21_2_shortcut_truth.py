"""Story 21.2 K12: ONE 80x24 native shortcut-truth smoke on a DEBUG binary.

Default-feature, non-API (no `requires_api`, no new marker): a loopback
Anthropic-shaped provider scripts every turn, so no credential or network is
used. The single scenario drives the real binary under a PTY:

* local TUI  - an empty Ctrl+P palette lists the catalogue at once; `/`+Esc
  leaves no residue; a bare `?` is literal text and `Ctrl+X ?` opens help; a
  multiline draft is sent by the portable `Ctrl+X Enter`; a held stream keeps
  receiving after an unknown slash, the leader menu and the palette; a pending
  plan does not eat typed `y`/`e`/`n` in Input; Chat `q` / Ctrl+Q raise the
  System/Quit decision, cancelling keeps the live work and an explicit `y`
  exits and closes the provider stream.
* attached client - the untimed Ctrl+X menu with its attached labels, Esc
  never detaches and keeps the draft, Ctrl+C stops the *daemon-side* turn
  (the provider stream closes) while the client stays usable, and Ctrl+X d
  detaches cleanly with the daemon still running.

Evidence (screens, raw PTY bytes, requests, runtime logs, binary hash) goes to
`RUSTAIN_CAPTURE_EVIDENCE_DIR`. Every wait is a bounded screen/event condition.
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
from keys import ALT_M, BACKSPACE, CTRL_C, CTRL_P, CTRL_X, ENTER, ESC
from test_story_20_9a_ephemeral_rows import _event, _finish, _plan_stream, _start_text

KEY = "sk-ant-21-2-loopback-key-canary"
PART_A = "K12-HELD-CHUNK-A"
PART_B = "K12-HELD-CHUNK-B"
HOLD_QUIT = "K12-QUIT-HELD-PARTIAL"
HOLD_ATTACHED = "K12-ATTACHED-HELD-PARTIAL"
ML_REPLY = "K12 multiline reply complete"
NEXT_REPLY = "K12 attached next reply complete"
PLAN_DONE = "K12 plan tool completed"
REJECT_ACK = "K12 rejection acknowledged"
# (marker in the last user message, request kind) - checked in this order.
MARKERS = (
    ("k12-ml", "ml"),
    ("k12-rel", "release"),
    ("k12-hq", "holdq"),
    ("k12-plan", "plan"),
    ("k12-ah", "ahold"),
    ("k12-an", "anext"),
)
ANSWERS = {
    "ml": ML_REPLY, "anext": NEXT_REPLY, "tool-result": PLAN_DONE,
    "rejection": REJECT_ACK, "auxiliary": "K12 session", "other": "K12 unexpected",
}
HELD = ("holdq", "ahold")


def _chunk(data):
    return f"{len(data):x}\r\n".encode() + data + b"\r\n"


class _NativeTUI(harness.RustainTUI):
    """Shared native spawn; PTY-driven waits only (no sleeps)."""

    def _sync_screen(self):
        self.check_server()
        try:
            data = self.child.read_nonblocking(size=65536, timeout=0.2)
        except pexpect.TIMEOUT:
            assert self.child.isalive(), "native TUI exited unexpectedly"
            return
        except pexpect.EOF:
            pytest.fail("native TUI exited before the expected screen")
        self._stream.feed(data)
        self.screens.write(json.dumps({"screen": "\n".join(self._screen.display)}) + "\n")
        self.screens.flush()

    def wait_for_screen(self, text, timeout=60, poll_interval=0.5):
        _wait(self, lambda screen: text in screen, text, timeout)
        return True

    def stop(self):
        if self._child is not None:
            if self.child.isalive():
                self.child.terminate(force=True)
            self.child.close(force=True)


def _wait(tui, predicate, label, timeout=60):
    deadline = time.monotonic() + timeout
    while True:
        screen = tui.get_screen_text()
        if predicate(screen):
            return screen
        assert time.monotonic() < deadline, f"Timed out waiting for {label}:\n{screen}"


def _palette_listed(screen):
    """The palette lists the whole catalogue with no typing (AC3): entries are
    shown (first is the `!` adapter override) with an `n/N` position, N the full
    count, and the mismatch notice is absent."""
    found = re.search(r"(?<!\d)1/(\d+)", screen)
    return ("Command Palette" in screen and "!cancel-plan" in screen and "No matches" not in screen
            and found is not None and int(found.group(1)) >= 40)



def _composer(screen):
    """Text inside the bottom-most bordered box (the composer), positional so a
    provider/model name or path elsewhere on the screen can never match."""
    lines = screen.split("\n")
    bottom = max((i for i, line in enumerate(lines) if line.strip().startswith("└")), default=None)
    if bottom is None:
        return ""
    top = max((i for i in range(bottom) if lines[i].strip().startswith("┌")), default=None)
    if top is None:
        return ""
    rows = [line.strip().strip("│").rstrip() for line in lines[top + 1:bottom]]
    return "\n".join(row.strip() for row in rows).strip()

def _composer_top(lines):
    """Index of the composer's top border: the nearest `┌` above the bottom-most `└`."""
    bottom = max((i for i, line in enumerate(lines) if line.strip().startswith("└")), default=None)
    if bottom is None:
        return None
    return max((i for i in range(bottom) if lines[i].strip().startswith("┌")), default=None)


def _status_row(screen):
    """The status bar row: the one directly above the composer box."""
    lines = screen.split("\n")
    top = _composer_top(lines)
    return lines[top - 1] if top else ""


def _menu(screen):
    """Text of the Ctrl+X leader box (the bordered box titled `Ctrl+X`), else ''.

    Menu items are painted whole (never split across rows), so `[P] Profile`,
    `[D] Detach` and `(n/a)` stay findable at 80 columns however the menu wraps."""
    lines = screen.split("\n")
    for i, line in enumerate(lines):
        if line.strip().startswith("┌") and "Ctrl+X" in line:
            end = next((j for j in range(i + 1, len(lines)) if lines[j].strip().startswith("└")), len(lines))
            return "\n".join(lines[i + 1:end])
    return ""


def _menu_tail_in_order(menu, attached):
    """Story 21.1 AC5: the leader menu lists `[N] Notifications` and `[X] Clear
    toasts` after `[?] Help` (local) / `[D] Detach` and `[?] Help` (attached)
    and before `[Enter] Send` / `[Esc] Cancel`, available (never `(n/a)`)."""
    order = ["[?] Help", "[N] Notifications", "[X] Clear toasts", "[Enter] Send", "[Esc] Cancel"]
    if attached:
        order.insert(0, "[D] Detach")
    positions = [menu.find(item) for item in order]
    assert all(p >= 0 for p in positions) and positions == sorted(positions), (order, positions, menu)
    assert "[N] Notifications (n/a)" not in menu and "[X] Clear toasts (n/a)" not in menu, menu


def _capture(tui, evidence, name, screen=None):
    screen = "\n".join(tui._screen.display) if screen is None else screen
    assert tui._screen.columns == 80 and len(tui._screen.display) == 24
    (evidence / f"{name}.screen.txt").write_text(screen + "\n")
    return screen


def _submit(tui, prompt):
    tui.send(prompt)
    _wait(tui, lambda screen: prompt in screen, f"typed {prompt!r}")
    tui.send(ENTER)


def _await_exit(tui, timeout=30):
    """Read until EOF, feeding pyte; return (exit status, signal status)."""
    deadline = time.monotonic() + timeout
    while True:
        try:
            data = tui.child.read_nonblocking(size=65536, timeout=1)
        except pexpect.TIMEOUT:
            assert time.monotonic() < deadline, "process did not exit"
            continue
        except pexpect.EOF:
            break
        tui._stream.feed(data)
    tui.child.close()
    return tui.child.exitstatus, tui.child.signalstatus


def _exercise(binary, evidence, root, monkeypatch):
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    errors = queue.Queue()
    requests = []
    lock = threading.Lock()
    release = threading.Event()
    disconnected = {kind: threading.Event() for kind in HELD}
    ws_local, ws_att = root / "wl", root / "wa"
    for ws in (ws_local, ws_att):
        (ws / ".rustain").mkdir(parents=True)
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

    def classify(body, text):
        if not body.get("tools"):
            return "auxiliary"
        if "Plan rejected by user" in text:
            return "rejection"
        for marker, kind in MARKERS:
            if marker in text:
                return kind
        return "tool-result" if "toolu_k5_plan" in text else "other"

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
                messages = body.get("messages", [])
                text = json.dumps(messages[-1].get("content", "") if messages else "", ensure_ascii=False)
                kind = classify(body, text)
                with lock:
                    requests.append({"kind": kind, "body": body})
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Transfer-Encoding", "chunked")
                self.end_headers()
                if kind == "plan":
                    self.wfile.write(_chunk(_plan_stream()))
                elif kind == "release":
                    self.wfile.write(_chunk(_start_text(PART_A)))
                    self.wfile.flush()
                    assert release.wait(90), "chunk B was never released"
                    tail = _event({"type": "content_block_delta", "index": 0,
                                   "delta": {"type": "text_delta", "text": PART_B}}) + _finish()
                    self.wfile.write(_chunk(tail))
                elif kind in HELD:
                    self.wfile.write(_chunk(_start_text(HOLD_QUIT if kind == "holdq" else HOLD_ATTACHED)))
                    self.wfile.flush()
                    self.connection.settimeout(120)
                    try:
                        tail = self.connection.recv(1)
                    except ConnectionResetError:
                        tail = b""  # cancellation may use a TCP reset
                    assert tail == b"", "held provider stream was not cancelled"
                    disconnected[kind].set()
                    self.close_connection = True
                    return
                else:
                    self.wfile.write(_chunk(_start_text(ANSWERS[kind]) + _finish()))
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
            release.set()  # never leave a held handler thread behind
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

        def kinds():
            with lock:
                return [r["kind"] for r in requests]

        def interrupted(kind):
            return disconnected[kind].is_set()

        # ------------------------------------------------------------ local TUI
        tui = start("local", ws_local, fresh=True, allowed_tools=["propose_plan"])

        # AC3: the first Ctrl+P lists the catalogue with no typing; reopen too;
        # a genuinely non-matching query still says No matches (positive control).
        tui.send(CTRL_P)
        screen = _wait(tui, _palette_listed, "palette entries")
        assert "No matches" not in screen, screen
        _capture(tui, evidence, "01-palette-first-open", screen)
        tui.send(ESC)
        _wait(tui, lambda s: "Command Palette" not in s, "palette closed")
        tui.send(CTRL_P)
        _wait(tui, _palette_listed, "palette entries on reopen")
        tui.send("zzqx")
        screen = _wait(tui, lambda s: "No matches" in s, "real mismatch says No matches")
        _capture(tui, evidence, "02-palette-no-matches", screen)
        tui.send(ESC)
        _wait(tui, lambda s: "Command Palette" not in s, "palette closed again")

        # AC4: `/` + filter then Esc leaves no residue and sends nothing.
        tui.send("/")
        _wait(tui, lambda s: "/clear" in s, "slash popup")
        tui.send("cl")
        tui.send(ESC)
        screen = _wait(tui, lambda s: "/clear" not in s and _composer(s) == "", "slash popup cancelled without residue")
        _capture(tui, evidence, "03-slash-esc", screen)
        assert not [k for k in kinds() if k != "auxiliary"], kinds()

        # AC5/AC12: bare `?` is text in Input; Ctrl+X ? is the help route.
        tui.send("what?")
        screen = _wait(tui, lambda s: "what?" in _composer(s), "literal ?")
        assert "Keybindings" not in screen, screen
        tui.send(BACKSPACE * 5)
        _wait(tui, lambda s: "what?" not in _composer(s), "literal text erased")
        tui.send(CTRL_X)
        screen = _wait(tui, lambda s: "[P] Profile" in _menu(s), "untimed leader menu (local)")
        _menu_tail_in_order(_menu(screen), attached=False)
        tui.send("?")
        screen = _wait(tui, lambda s: "Keybindings" in s, "Ctrl+X ? opens help")
        _capture(tui, evidence, "04-help", screen)
        tui.send(ESC)
        _wait(tui, lambda s: "Keybindings" not in s, "help closed")
        tui.send("rt9")
        _wait(tui, lambda s: "rt9" in _composer(s), "Input focus restored after help")
        tui.send(BACKSPACE * 3)
        _wait(tui, lambda s: "rt9" not in _composer(s), "probe erased")

        # AC12: Alt+M multiline; plain Enter is a newline; Ctrl+X Enter sends once.
        tui.send(ALT_M)
        tui.send("k12-ml first")
        _wait(tui, lambda s: "k12-ml first" in _composer(s), "first line")
        tui.send(ENTER)
        tui.send("line two")
        screen = _wait(tui, lambda s: "line two" in _composer(s) and "k12-ml first" in _composer(s),
                       "both lines, nothing sent")
        _capture(tui, evidence, "05-multiline-draft", screen)
        assert "ml" not in kinds(), kinds()
        tui.send(CTRL_X)
        _wait(tui, lambda s: "[P] Profile" in _menu(s), "leader menu before Enter")
        tui.send(ENTER)
        screen = _wait(tui, lambda s: ML_REPLY in s and "Ready" in _status_row(s), "portable send reply")
        _capture(tui, evidence, "06-multiline-sent", screen)
        assert kinds().count("ml") == 1, kinds()
        with lock:
            sent = [r for r in requests if r["kind"] == "ml"][0]["body"]["messages"][-1]["content"]
        sent_text = json.dumps(sent, ensure_ascii=False)
        assert "k12-ml first" in sent_text and "line two" in sent_text, sent_text
        tui.send(ALT_M)

        # AC2/AC3/AC6: a held stream keeps receiving after an unknown slash,
        # the leader menu and the palette; no extra provider request exists.
        _submit(tui, "k12-rel start")
        _wait(tui, lambda s: PART_A in s, "held chunk A")
        tui.send("/nosuchcmd ")
        _wait(tui, lambda s: "/nosuchcmd" in _composer(s), "unknown slash typed")
        tui.send(ENTER)
        # The composer is consumed; an echoed warning may legitimately repeat the name.
        _wait(tui, lambda s: "/nosuchcmd" not in _composer(s), "unknown slash consumed")
        # Story 21.1: the Advisory is a Warning toast on the chat pane's bottom
        # row (positional, right-aligned, severity word first; 10 s TTL).
        unknown = "Unknown command: /nosuchcmd — not a built-in or a custom command."
        screen = _wait(tui, lambda s: harness.toast_present(s, unknown, "Warning"),
                       "unknown-command Warning toast")
        _capture(tui, evidence, "07a-unknown-slash-toast", screen)
        tui.send(CTRL_X)
        screen = _wait(tui, lambda s: "[P] Profile" in _menu(s), "leader menu over a live stream")
        _capture(tui, evidence, "07-menu-over-stream", screen)
        tui.send(ESC)
        _wait(tui, lambda s: "[P] Profile" not in _menu(s), "leader menu closed")
        # `Ctrl+X x` archives the visible toast so the 10 s Warning cannot sit
        # over the transcript's bottom row while the held stream is asserted.
        tui.send(CTRL_X)
        _wait(tui, lambda s: "[X] Clear toasts" in _menu(s), "leader menu lists Clear toasts")
        tui.send("x")
        _wait(tui, lambda s: "[X] Clear toasts" not in _menu(s)
              and not harness.toast_present(s, unknown, "Warning"), "toast cleared by Ctrl+X x")
        tui.send(CTRL_P)
        _wait(tui, _palette_listed, "palette over a live stream")
        tui.send(ESC)
        _wait(tui, lambda s: "Command Palette" not in s, "palette closed over a stream")
        assert kinds().count("release") == 1 and PART_A in tui.get_screen_text()
        release.set()
        screen = _wait(tui, lambda s: PART_A + PART_B in s and "Ready" in _status_row(s),
                       "chunk A and B retained, turn completes")
        _capture(tui, evidence, "08-stream-continued", screen)
        with lock:
            assert not any("nosuchcmd" in json.dumps(r["body"]) for r in requests), "unknown slash reached a provider"

        # AC10: pending plan letters are text in Input; Chat `n` still rejects.
        _submit(tui, "k12-plan propose")
        screen = _wait(tui, lambda s: "Plan:" in s, "plan card rendered")
        _capture(tui, evidence, "09a-plan-card", screen)
        _wait(tui, lambda s: PLAN_DONE in s and "Ready" in _status_row(s), "plan tool turn finished")
        _wait(tui, lambda _s: not tui._screen.cursor.hidden, "Input focus under the plan card")
        tui.send("yen-k12")
        screen = _wait(tui, lambda s: "yen-k12" in _composer(s), "y/e/n typed as text")
        assert "rejection" not in kinds(), kinds()
        _capture(tui, evidence, "09-plan-letters-are-text", screen)
        tui.send(BACKSPACE * 7)
        _wait(tui, lambda s: "yen-k12" not in _composer(s), "typed letters erased")
        tui.send(ESC)
        _wait(tui, lambda _s: tui._screen.cursor.hidden, "Chat focus")
        tui.send("n")
        screen = _wait(tui, lambda s: REJECT_ACK in s and "Ready" in _status_row(s), "Chat n rejected the exact plan")
        _capture(tui, evidence, "10-plan-rejected-in-chat", screen)
        assert kinds().count("rejection") == 1, kinds()
        # Focus is observed, never assumed: a stray `i` would land in the composer.
        if tui._screen.cursor.hidden:
            tui.send("i")
        _wait(tui, lambda _s: not tui._screen.cursor.hidden, "Input focus restored")

        # AC7: quit is guarded; Chat q and Ctrl+Q ask, cancelling keeps the live
        # stream, only an explicit y exits and then the provider stream closes.
        _submit(tui, "k12-hq hold")
        _wait(tui, lambda s: HOLD_QUIT in s, "quit-held partial")
        tui.send("\x11")
        screen = _wait(tui, lambda s: "System · Quit" in s and "will be interrupted" in s, "Ctrl+Q decision")
        # B-6: the REJECTED plan is not an open decision (found by the first smoke run).
        assert "pending decision" not in screen, screen
        _capture(tui, evidence, "11-quit-ctrl-q", screen)
        tui.send("n")
        screen = _wait(tui, lambda s: "System · Quit" not in s and HOLD_QUIT in s, "n cancels, work kept")
        assert not interrupted("holdq")
        tui.send(ESC)
        _wait(tui, lambda _s: tui._screen.cursor.hidden, "Chat focus before q")
        tui.send("q")
        screen = _wait(tui, lambda s: "System · Quit" in s, "Chat q decision")
        _capture(tui, evidence, "12-quit-chat-q", screen)
        tui.send(ESC)
        _wait(tui, lambda s: "System · Quit" not in s and HOLD_QUIT in s, "Esc cancels, work kept")
        assert not interrupted("holdq")
        tui.send("\x11")
        _wait(tui, lambda s: "System · Quit" in s, "second Ctrl+Q decision")
        tui.send("y")
        assert _await_exit(tui) == (0, None)
        assert disconnected["holdq"].wait(30), "explicit y did not close the provider stream"
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

        def daemon_running():
            status = subprocess.run([str(binary), "daemon", "status"], cwd=ws_att,
                                    env={**os.environ, **envs["daemon"]}, capture_output=True, text=True, timeout=10)
            return status.returncode == 0 and "running" in (status.stdout + status.stderr).lower(), status

        deadline = time.monotonic() + 60
        while True:
            running, status = daemon_running()
            if running:
                break
            try:
                daemon.wait(timeout=0.25)  # structural pacing: also notices a dead daemon
                raise AssertionError("daemon exited before becoming ready")
            except subprocess.TimeoutExpired:
                pass
            assert time.monotonic() < deadline, status.stdout + status.stderr

        tui = start("attach", ws_att, fresh=False, extra_args=["daemon", "attach"])
        screen = _wait(tui, lambda s: "attached" in _status_row(s), "attached status chip")
        _capture(tui, evidence, "13-attached", screen)

        # AC6/AC13: the untimed attached leader menu keeps every retained chord
        # painted (qualified when unavailable) and Esc never detaches.
        tui.send(CTRL_X)
        screen = _wait(tui, lambda s: "[D] Detach" in _menu(s) and "(n/a)" in _menu(s), "attached leader menu")
        _menu_tail_in_order(_menu(screen), attached=True)
        _capture(tui, evidence, "14-attached-menu", screen)
        tui.send(ESC)
        _wait(tui, lambda s: "[D] Detach" not in _menu(s) and "attached" in _status_row(s), "menu closed, still attached")
        tui.send("att-draft")
        _wait(tui, lambda s: "att-draft" in _composer(s), "attached draft typed")
        tui.send(ESC)
        screen = _wait(tui, lambda _s: tui._screen.cursor.hidden, "Esc moved to Chat, not away")
        assert "att-draft" in _composer(screen) and tui.child.isalive(), screen
        tui.send(ESC)
        _wait(tui, lambda _s: not tui._screen.cursor.hidden, "Esc returned to Input")
        tui.send(BACKSPACE * 9)
        _wait(tui, lambda s: "att-draft" not in _composer(s), "attached draft erased")

        # AC8: Ctrl+C stops the daemon-side turn; the client stays attached.
        _submit(tui, "k12-ah hold")
        _wait(tui, lambda s: HOLD_ATTACHED in s, "daemon-held partial")
        tui.send(CTRL_C)
        assert disconnected["ahold"].wait(30), "attached Ctrl+C did not stop the daemon turn"
        check_server()
        assert tui.child.isalive()
        _submit(tui, "k12-an next")
        screen = _wait(tui, lambda s: NEXT_REPLY in s and "attached" in _status_row(s), "next turn completes on the same socket")
        _capture(tui, evidence, "15-attached-after-stop", screen)

        # AC13: Ctrl+X d detaches this client only; the daemon keeps running.
        tui.send(CTRL_X)
        _wait(tui, lambda s: "[D] Detach" in _menu(s), "leader menu before d")
        tui.send("d")
        assert _await_exit(tui) == (0, None)
        running, status = daemon_running()
        assert running and daemon.poll() is None, status.stdout + status.stderr

        # ------------------------------------------------------ evidence checks
        with lock:
            observed = list(requests)
        (evidence / "requests.json").write_text(json.dumps(observed, indent=2) + "\n")
        counts = {}
        for row in observed:
            counts[row["kind"]] = counts.get(row["kind"], 0) + 1
        assert "other" not in counts, counts
        for kind in ("ml", "release", "plan", "tool-result", "rejection", "holdq", "ahold", "anext"):
            assert counts.get(kind) == 1, counts
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
        (evidence / "counts.json").write_text(json.dumps(counts, indent=2) + "\n")


@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(480)
def test_shortcut_truth_local_and_attached_80x24(monkeypatch):
    binary = harness._resolve_binary()[0].resolve()
    assert binary.is_file() and "debug" in binary.parts and "release" not in binary.parts, binary
    parent = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())).resolve()
    parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="21-2-", dir=parent))
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    manifest = {"binary": str(binary), "sha256": digest, "bytes": binary.stat().st_size, "viewport": [80, 24],
                "started_at": datetime.now(timezone.utc).isoformat(), "result": "RUNNING"}
    try:
        # Short root: the daemon socket path must fit sockaddr_un.
        with tempfile.TemporaryDirectory(prefix="r212-", dir="/tmp") as root:
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
        print(f"NATIVE_21_2_EVIDENCE={evidence}", flush=True)
