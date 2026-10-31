"""TUI sign-in on a real debug binary at 80x24, loopback only.

Phase B: a locked configured provider is signed into from the model picker
(masked bracketed-paste key), the picker reopens while discovery is held,
blooms with the live roster and the CLI discovery line, and a turn reaches the
provider with the new key. Phase C: the Anthropic browser login shows its
waiting view with an OSC 8 link and the local shortcut, opens the launch URL,
copies it with Alt+L (OSC 52), and Esc cancels it with the callback port closed.
"""
from base64 import b64encode
from contextlib import ExitStack
from datetime import datetime, timezone
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import socket
import stat
import tempfile
import threading
import traceback
from urllib.parse import urlsplit

import harness
import pytest

from test_story_20_1c_locked_providers import (
    PROVIDER_ENV, _NativeTUI, _capture, _plain, _sensitive_env, _sha256, _wait,
)

KEY = "sk-native-stubai-20-1e-SECRET"
LIVE = ("stub-a", "stub-b", "stub-c")


def _modal(screen, title):
    """True once a whole bordered modal titled `title` is painted."""
    rows = screen.split("\n")
    top = next((y for y, row in enumerate(rows) if f"┌ {title}" in row), None)
    if top is None:
        return False
    left = rows[top].index(f"┌ {title}")
    right = rows[top].rfind("┐")
    if right <= left:
        return False
    bottom = next((y for y in range(top + 1, len(rows))
                   if len(rows[y]) > right and rows[y][left] == "└"), None)
    if bottom is None or rows[bottom][right] != "┘":
        return False
    return all(len(rows[y]) > right and rows[y][left] == "│" and rows[y][right] == "│"
               for y in range(top + 1, bottom))


def _link_rows(screen, label):
    """The visible URL rows of one link: `(y, x, text)` per row. A link that fits
    one row reads `label value`; a wrapped one is its label row alone followed by
    unindented rows that run until a blank or non-URL row. The modal's own side
    borders bound every row (the chat underneath may draw others)."""
    rows = screen.split("\n")
    needle = label.rstrip()
    matches = [y for y, row in enumerate(rows) if needle in row]
    assert len(matches) == 1, (label, matches)
    y = matches[0]
    label_end = rows[y].index(needle) + len(needle)
    left = rows[y].rfind("│", 0, rows[y].index(needle))
    right = rows[y].find("│", label_end)
    assert left >= 0 and right > label_end, rows[y]
    after = rows[y][label_end:right]
    if after.strip():
        text = after.strip()
        return [(y, rows[y].index(text, label_end), text)]
    found = []
    for follow in range(y + 1, len(rows)):
        inner = rows[follow][left + 1:right].rstrip()
        if not inner or " " in inner:
            break
        found.append((follow, left + 1, inner))
    return found


def _assert_link_cells(tui, screen, label, target, raw_text):
    """Compare every visible link glyph, on every row, with its balanced on-wire
    OSC 8 cell; the rows rejoin to the full target."""
    found = _link_rows(screen, label)
    assert found, (label, screen)
    assert "".join(text for _, _, text in found) == target, (label, found)
    for y, start, visible in found:
        for x, glyph in enumerate(visible, start):
            cell = tui._screen.buffer[y][x]
            assert cell.data == glyph
            assert (cell.fg, cell.bg, cell.bold) == ("00c8c8", "282828", True), (label, x, cell)

    # One fully balanced wrapper per glyph; neither a whole-row wrapper nor
    # a target shortened to the visible ellipsis can satisfy this comparison.
    symbol = re.compile(re.escape("\x1b]8;;" + target + "\x07")
                        + r"([^\x00-\x1f\x7f])"
                        + re.escape("\x1b]8;;\x07"))
    symbols = list(symbol.finditer(raw_text))
    assert len(symbols) == raw_text.count("\x1b]8;;" + target + "\x07"), label
    glyphs = "".join(match.group(1) for match in symbols)
    assert target in glyphs, (label, target, glyphs)
    return len(found)


def _launch_target(port):
    """Read the real loopback redirect without following it onto the network."""
    connection = HTTPConnection("127.0.0.1", port, timeout=5)
    try:
        connection.request("GET", "/launch")
        response = connection.getresponse()
        assert response.status == 302, response.status
        target = response.getheader("Location")
        response.read()
        assert target and target.startswith("https://claude.ai/oauth/authorize?"), target
        return target
    finally:
        connection.close()

def _exercise(binary, evidence, root, monkeypatch):
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    workspace = root / "fresh-cwd"
    workspace.mkdir()
    home = root / "home"
    config = home / "config"
    data = home / "data"
    config.mkdir(parents=True)
    data.mkdir()
    bin_dir = root / "bin"
    bin_dir.mkdir()
    opener_log = evidence / "opener.log"
    opener = bin_dir / "xdg-open"
    opener.write_text(f'#!/bin/sh\nprintf "%s\\n" "$*" >> "{opener_log}"\n')
    opener.chmod(opener.stat().st_mode | stat.S_IXUSR)
    requests = []
    handler_errors = []
    lock = threading.Lock()
    model_gets = []
    discovery_release = threading.Event()
    discovery_held = threading.Event()

    class Stub(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def _reply(self, status, body, content_type="application/json"):
            self.send_response(status)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def _record(self, status):
            with lock:
                requests.append({"method": self.command, "path": self.path, "status": status,
                                 "key_received": self.headers.get("Authorization") == f"Bearer {KEY}"})

        def do_GET(self):
            path = urlsplit(self.path).path
            if path == "/stubai/v1/models":
                with lock:
                    model_gets.append(path)
                    count = len(model_gets)
                ids = LIVE[:1] if count == 1 else LIVE
                if count == 2:
                    # The post-sign-in discovery waits until the reopened picker is seen.
                    discovery_held.set()
                    discovery_release.wait(timeout=60)
                self._record(200)
                return self._reply(200, json.dumps({"object": "list", "data": [
                    {"id": model, "object": "model"} for model in ids]}).encode())
            if path == "/api.json":
                self._record(200)
                return self._reply(200, b"{}")
            self._record(404)
            self._reply(404, b'{"error":"unexpected route"}')

        def do_POST(self):
            length = int(self.headers.get("Content-Length") or 0)
            body = self.rfile.read(length).decode()
            path = urlsplit(self.path).path
            if path == "/stubai/v1/chat/completions":
                self._record(200)
                chunk = {"id": "c", "object": "chat.completion.chunk", "model": "stub-b",
                         "choices": [{"index": 0, "delta": {"role": "assistant", "content": "pong"},
                                      "finish_reason": None}]}
                done = {"id": "c", "object": "chat.completion.chunk", "model": "stub-b",
                        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}
                with lock:
                    requests[-1]["model"] = json.loads(body).get("model")
                sse = f"data: {json.dumps(chunk)}\n\ndata: {json.dumps(done)}\n\ndata: [DONE]\n\n"
                return self._reply(200, sse.encode(), "text/event-stream")
            self._record(404)
            self._reply(404, b'{"error":"unexpected route"}')

    class CheckedServer(ThreadingHTTPServer):
        daemon_threads = False

        def handle_error(self, request, client_address):
            with lock:
                handler_errors.append(traceback.format_exc())

    with ExitStack() as stack:
        server = CheckedServer(("127.0.0.1", 0), Stub)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()

        def stop_server():
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)
            assert not thread.is_alive(), "HTTP server did not stop"
            assert not handler_errors, "\n".join(handler_errors)

        stack.callback(stop_server)
        stack.callback(discovery_release.set)
        origin = f"http://127.0.0.1:{server.server_address[1]}"
        (config / "config.toml").write_text(
            '[provider.stubai]\nprovider_id = "stubai"\nkind = "openai-compatible"\n'
            'model_id = "stub-a"\napi_key_env = "STUBAI_API_KEY"\n'
            f'base_url = "{origin}/stubai/v1"\nenabled = true\n'
            'discover_models = true\nmodel_filter = ["*"]\n')
        loaded = harness._load_env()
        cleared = (PROVIDER_ENV | {"STUBAI_API_KEY"}
                   | {name for name in loaded if _sensitive_env(name)}) - {"NO_PROXY"}
        env = {name: "" for name in cleared}
        runtime_log = evidence / "native.runtime.log"
        env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "xdg-config"),
                   XDG_DATA_HOME=str(home / "xdg-data"), RUSTAIN_CONFIG_DIR=str(config),
                   RUSTAIN_DATA_DIR=str(data), RUSTAIN_LOG_PATH=str(runtime_log),
                   RUSTAIN_MODELS_DEV_URL=origin, TERM="xterm-256color",
                   TERM_PROGRAM="wezterm", TMUX="", STY="", COLORTERM="truecolor", NO_COLOR="",
                   NO_PROXY="127.0.0.1,localhost",
                   PATH=f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
        original_spawn = harness.pexpect.spawn

        def safe_spawn(command, *args, **kwargs):
            actual = kwargs["env"]
            actual.pop("NO_COLOR", None)
            assert Path(command).resolve() == binary and "debug" in binary.parts and "release" not in binary.parts
            assert kwargs["dimensions"] == (24, 80)
            assert all(actual.get(name) == "" for name in cleared)
            assert actual["RUSTAIN_DATA_DIR"] == str(data)
            assert actual["PATH"].split(os.pathsep)[0] == str(bin_dir)
            assert (actual["TERM"], actual["TERM_PROGRAM"], actual["TMUX"], actual["STY"],
                    actual["COLORTERM"]) == ("xterm-256color", "wezterm", "", "", "truecolor")
            return original_spawn(command, *args, **kwargs)

        monkeypatch.setattr(harness.pexpect, "spawn", safe_spawn)
        tui = _NativeTUI(binary=binary, build=False, workspace=workspace, env_overrides=env)
        tui.screens = stack.enter_context((evidence / "native.screens.jsonl").open("w"))
        raw = stack.enter_context((evidence / "native.raw.bin").open("w", encoding="utf-8", newline=""))
        stack.callback(tui.stop)
        tui.start()
        tui.child.delaybeforesend = 0
        # Received bytes only: the key typed into the PTY is never teed.
        tui.child.logfile_read = raw

        # ── Phase B: locked → sign in → bloom → turn ──────────────────────
        tui.send("\x18")
        tui.send("m")
        screen = _wait(tui, lambda s: _modal(s, "Select Model") and "stubai · not signed in" in s
                       and "Enter sign in" in s, "locked stubai with Enter sign in")
        _capture(tui, evidence, "b1-locked-stubai", screen)
        tui.send("\r")
        screen = _wait(tui, lambda s: _modal(s, "Sign in to stubai") and "Enter stubai API key:" in s,
                       "masked key field")
        _capture(tui, evidence, "b2-key-field", screen)
        tui.send(f"\x1b[200~{KEY}\x1b[201~")
        screen = _wait(tui, lambda s: "> " + "•" * len(KEY) in _plain(s).replace(" ", "")
                       or "•" * len(KEY) in s, "masked bullets")
        assert KEY not in screen
        _capture(tui, evidence, "b3-masked", screen)
        tui.send("\r")
        assert discovery_held.wait(timeout=60), "post-sign-in discovery never started"
        screen = _wait(tui, lambda s: _modal(s, "Select Model") and "stubai ✓ connected" in s
                       and "Discovering models for stubai" in s,
                       "reopened picker on stubai while discovery is held")
        _capture(tui, evidence, "b4-discovering", screen)
        discovery_release.set()
        screen = _wait(tui, lambda s: _modal(s, "Select Model") and all(m in s for m in LIVE)
                       and "Discovered 3 models for stubai." in _plain(s), "bloom with live roster")
        _capture(tui, evidence, "b5-bloom", screen)
        # Select stub-b: the cursor starts on stub-a.
        tui.send("\x1b[B")
        _wait(tui, lambda s: "▸ stub-b" in s, "stub-b selected")
        tui.send("\r")
        _wait(tui, lambda s: "Select Model" not in s and "stub-b" in s, "stub-b active")
        tui.send("hello")
        tui.send("\r")
        screen = _wait(tui, lambda s: "pong" in s, "the stub's reply rendered")
        _capture(tui, evidence, "b6-pong", screen)
        with lock:
            chats = [row for row in requests if row["path"].startswith("/stubai/v1/chat")]
        assert chats and chats[-1]["key_received"] and chats[-1]["model"] == "stub-b", chats

        # ── Phase C: browser waiting view, link, opener, Alt+L, Esc ───────
        tui.send("/login anthropic")
        tui.send("\r")
        _wait(tui, lambda s: "Claude Pro/Max subscription (browser)" in s, "method picker")
        tui.send("\r")
        screen = _wait(tui, lambda s: _modal(s, "Sign in to Anthropic")
                       and "Local shortcut (this machine only): http://localhost:" in s
                       and "Paste the authorization code (or full redirect URL):" in _plain(s)
                       and any(re.search(r"│> ", row) for row in s.split("\n"))
                       and "Alt+L copy local link" in _plain(s),
                       "complete browser waiting view with pending field")
        _capture(tui, evidence, "c1-waiting", screen)
        launch = re.search(r"http://localhost:(\d+)/launch", screen)
        assert launch, screen
        port = int(launch.group(1))
        target = _launch_target(port)
        raw.flush()
        wire = (evidence / "native.raw.bin").read_text(encoding="utf-8")
        _assert_link_cells(tui, screen, "Sign-in link: ", target, wire)
        _assert_link_cells(tui, screen, "Local shortcut (this machine only): ", launch.group(0), wire)
        assert wire.count("\x1b]8;;\x07") == (
            wire.count("\x1b]8;;" + target + "\x07")
            + wire.count("\x1b]8;;" + launch.group(0) + "\x07"))
        _wait(tui, lambda s: opener_log.exists() and launch.group(0) in opener_log.read_text(),
              "opener got the launch URL")
        tui.send("\x1bl")
        osc52 = "\x1b]52;c;" + b64encode(launch.group(0).encode()).decode()
        _wait(tui, lambda s: (raw.flush() or True) and osc52 in
              (evidence / "native.raw.bin").read_text(encoding="utf-8"), "OSC 52 copy of the launch URL")
        tui.send("\x1b")
        screen = _wait(tui, lambda s: "Login cancelled" in s
                       and "Sign in to Anthropic" not in s and "Local shortcut" not in s,
                       "Login cancelled with the modal removed")
        _capture(tui, evidence, "c2-cancelled", screen)
        with socket.socket() as probe:
            probe.settimeout(5)
            assert probe.connect_ex(("127.0.0.1", port)) != 0, "callback port still open"

        with lock:
            observed = list(requests)
        (evidence / "requests.json").write_text(json.dumps(observed, indent=2) + "\n")
        assert all(urlsplit(row["path"]).path in {"/stubai/v1/models", "/stubai/v1/chat/completions",
                                                   "/api.json"} for row in observed), observed
        raw.flush()
        for name in ("native.screens.jsonl", "native.raw.bin"):
            assert KEY not in (evidence / name).read_text(encoding="utf-8"), name
    # The appender writes a dated file and flushes on exit: check every runtime
    # log once the TUI has stopped, and require that one was written.
    logs = sorted(evidence.glob("native.runtime.log*"))
    assert any(re.fullmatch(r"native\.runtime\.log\.\d{4}-\d{2}-\d{2}", log.name)
               and log.is_file() for log in logs), "dated runtime log missing"
    for log in logs:
        assert KEY not in log.read_text(encoding="utf-8", errors="replace"), log.name


@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(240)
def test_tui_login_bloom_and_browser_cancel_80x24(monkeypatch):
    parent = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())).resolve()
    parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="20-1e-", dir=parent))
    manifest = {"run_id": evidence.name, "started_at": datetime.now(timezone.utc).isoformat(),
                "evidence_dir": str(evidence), "viewport": [80, 24], "phases": ["B", "C"],
                "result": "RUNNING"}
    try:
        binary = harness._resolve_binary()[0].resolve()
        assert binary.is_file() and "debug" in binary.parts and "release" not in binary.parts, binary
        manifest.update(binary=str(binary), binary_sha256=_sha256(binary))
        (evidence / "run.json").write_text(json.dumps(manifest, indent=2) + "\n")
        with tempfile.TemporaryDirectory(prefix="r20-1e-") as root:
            _exercise(binary, evidence, Path(root), monkeypatch)
        assert _sha256(binary) == manifest["binary_sha256"], "binary changed during native smoke"
        manifest["result"] = "PASS"
    except BaseException:
        manifest["result"] = "FAIL"
        (evidence / "failure.txt").write_text(traceback.format_exc())
        raise
    finally:
        manifest["completed_at"] = datetime.now(timezone.utc).isoformat()
        (evidence / "run.json").write_text(json.dumps(manifest, indent=2) + "\n")
        print(f"NATIVE_20_1E_EVIDENCE={evidence}", flush=True)
