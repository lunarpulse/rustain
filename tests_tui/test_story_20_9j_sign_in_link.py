"""20-9j native smoke (K7): the Anthropic browser sign-in link on a real debug
binary at 80x24, loopback only, in two spawns of the same binary.

(a) `TERM=xterm-256color TERM_PROGRAM=herdr TMUX= STY=`: OSC 8 is default-on, so
    every URL row arrives as balanced per-cell envelopes targeting the `/launch`
    302 Location and the rows rejoin to that URL.
(b) `TERM=xterm-256color STY=20-9j` (GNU screen): zero OSC 8 bytes, the rows rejoin
    to the URL without `https://`, no row holds `https://`, a herdr-rule detector
    extracts nothing partial, the lead names Alt+C and Alt+C puts the full URL on
    the OSC 52 clipboard.

Both: Esc cancels with the callback port closed; the raw bytes, screens and the
dated runtime log never carry a pasted authorization-code canary.
"""
from base64 import b64encode
from contextlib import ExitStack
from datetime import datetime, timezone
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
from test_story_20_1e_tui_login import _assert_link_cells, _launch_target, _link_rows, _modal

CANARY = "AUTHCODE-CANARY-20-9j-SECRET"
OSC8 = re.compile(r"\x1b\]8;[^\x07]*\x07")


def _herdr_urls(row):
    """herdr's plain-URL rule (`src/app/actions.rs:2125-2138,2305-2340`)."""
    cells = list(row)
    urls, start = [], 0
    while start < len(cells):
        head = "".join(cells[start:start + 8])
        if not (head.startswith("http://") or head.startswith("https://")):
            start += 1
            continue
        scan = start
        while scan + 1 < len(cells) and not cells[scan + 1].isspace():
            scan += 1
        end = scan
        while start <= end:
            last = cells[end]
            if last in "\"'`.,;:!?":
                trim = True
            elif last in ")]}":
                opening, closing = {")": "()", "]": "[]", "}": "{}"}[last]
                balance = sum(1 if c == opening else -1 if c == closing else 0
                              for c in cells[start:end])
                trim = not balance > 0
            else:
                trim = False
            if not trim:
                break
            end -= 1
        if start <= end:
            urls.append("".join(cells[start:end + 1]))
        start = scan + 1
    return urls


def _lead(screen):
    """Every inner row above the first link label, joined: the lead notice."""
    words = []
    for row in screen.split("\n"):
        if "Sign-in link:" in row:
            break
        words.append(row.replace("│", " ").strip())
    return " ".join(word for word in words if word)


def _spawn_case(name, binary, evidence, root, original_spawn, monkeypatch, expected):
    """One spawn: returns the byte count of OSC 8 envelopes of the first full paint."""
    workspace = root / "fresh-cwd"
    workspace.mkdir()
    home = root / "home"
    config = home / "config"
    data = home / "data"
    config.mkdir(parents=True)
    data.mkdir()
    bin_dir = root / "bin"
    bin_dir.mkdir()
    opener_log = evidence / f"{name}.opener.log"
    opener = bin_dir / "xdg-open"
    opener.write_text(f'#!/bin/sh\nprintf "%s\\n" "$*" >> "{opener_log}"\n')
    opener.chmod(opener.stat().st_mode | stat.S_IXUSR)
    requests = []
    handler_errors = []
    lock = threading.Lock()

    class Stub(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_GET(self):
            ok = urlsplit(self.path).path == "/api.json"
            with lock:
                requests.append({"method": "GET", "path": self.path, "status": 200 if ok else 404})
            body = b"{}" if ok else b'{"error":"unexpected route"}'
            self.send_response(200 if ok else 404)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

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
        origin = f"http://127.0.0.1:{server.server_address[1]}"
        loaded = harness._load_env()
        cleared = (PROVIDER_ENV | {"STUBAI_API_KEY"}
                   | {key for key in loaded if _sensitive_env(key)}) - {"NO_PROXY"}
        env = {key: "" for key in cleared}
        runtime_log = evidence / f"{name}.runtime.log"
        env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "xdg-config"),
                   XDG_DATA_HOME=str(home / "xdg-data"), RUSTAIN_CONFIG_DIR=str(config),
                   RUSTAIN_DATA_DIR=str(data), RUSTAIN_LOG_PATH=str(runtime_log),
                   RUSTAIN_MODELS_DEV_URL=origin, COLORTERM="truecolor", NO_COLOR="",
                   NO_PROXY="127.0.0.1,localhost",
                   PATH=f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}",
                   **expected)

        def safe_spawn(command, *args, **kwargs):
            actual = kwargs["env"]
            actual.pop("NO_COLOR", None)
            assert Path(command).resolve() == binary and "debug" in binary.parts and "release" not in binary.parts
            assert kwargs["dimensions"] == (24, 80)
            assert all(actual.get(key) == "" for key in cleared)
            assert actual["RUSTAIN_DATA_DIR"] == str(data)
            assert actual["PATH"].split(os.pathsep)[0] == str(bin_dir)
            assert {key: actual[key] for key in expected} == expected
            assert actual["COLORTERM"] == "truecolor"
            return original_spawn(command, *args, **kwargs)

        monkeypatch.setattr(harness.pexpect, "spawn", safe_spawn)
        tui = _NativeTUI(binary=binary, build=False, workspace=workspace, env_overrides=env)
        tui.screens = stack.enter_context((evidence / f"{name}.screens.jsonl").open("w"))
        raw_path = evidence / f"{name}.raw.bin"
        raw = stack.enter_context(raw_path.open("w", encoding="utf-8", newline=""))
        stack.callback(tui.stop)
        tui.start()
        tui.child.delaybeforesend = 0
        # Received bytes only: the pasted canary typed into the PTY is never teed.
        tui.child.logfile_read = raw

        tui.send("/login anthropic")
        tui.send("\r")
        _wait(tui, lambda s: "Claude Pro/Max subscription (browser)" in s, "method picker")
        raw.flush()
        before = len(raw_path.read_bytes())
        tui.send("\r")
        screen = _wait(tui, lambda s: _modal(s, "Sign in to Anthropic")
                       and "Local shortcut (this machine only): http://localhost:" in s
                       and "Paste the authorization code (or full redirect URL):" in _plain(s)
                       and any(re.search(r"│> ", row) for row in s.split("\n"))
                       and "Alt+L copy local link" in _plain(s),
                       "complete browser waiting view with pending field")
        raw.flush()
        first_paint = raw_path.read_bytes()[before:].decode("utf-8")
        _capture(tui, evidence, f"{name}-waiting", screen)
        launch = re.search(r"http://localhost:(\d+)/launch", screen)
        assert launch, screen
        port = int(launch.group(1))
        target = _launch_target(port)
        _wait(tui, lambda s: opener_log.exists() and launch.group(0) in opener_log.read_text(),
              "opener got the launch URL")
        found = _link_rows(screen, "Sign-in link: ")
        assert len(found) > 1, ("the real authorize URL wraps", found)
        osc8_bytes = sum(len(match.group(0).encode()) for match in OSC8.finditer(first_paint))
        osc8_open = first_paint.count("\x1b]8;;" + target + "\x07")
        osc8_close = first_paint.count("\x1b]8;;\x07")
        if name == "a-herdr":
            # Every URL row of both links arrives as balanced per-cell envelopes.
            rows = _assert_link_cells(tui, screen, "Sign-in link: ", target, first_paint)
            assert rows == len(found)
            _assert_link_cells(tui, screen, "Local shortcut (this machine only): ",
                               launch.group(0), first_paint)
            assert osc8_open >= len(target), (osc8_open, len(target))
            assert osc8_close == osc8_open + first_paint.count("\x1b]8;;" + launch.group(0) + "\x07")
            assert "If it didn't open, open or copy the link below." in _plain(screen)
        else:
            assert raw_path.read_bytes().count(b"\x1b]8;") == 0, "GNU screen gets no OSC 8 bytes"
            assert osc8_bytes == 0
            displayed = "".join(text for _, _, text in found)
            assert displayed == target.removeprefix("https://"), (displayed, target)
            assert "https://" not in "\n".join(screen.split("\n")), screen
            extracted = [url for row in screen.split("\n") for url in _herdr_urls(row)]
            assert extracted == [launch.group(0)], ("only the complete local shortcut", extracted)
            assert "press Alt+C to copy the full sign-in link." in _lead(screen), _lead(screen)
            tui.send("\x1bc")
            osc52 = "\x1b]52;c;" + b64encode(target.encode()).decode()
            _wait(tui, lambda s: (raw.flush() or True) and osc52 in raw_path.read_text(encoding="utf-8"),
                  "OSC 52 copy of the full sign-in URL")
            # The payload is the full URL, not the scheme-less rows.
            assert b64encode(displayed.encode()).decode() not in raw_path.read_text(encoding="utf-8")
        # A pasted authorization code stays masked and out of every artifact.
        tui.send(f"\x1b[200~{CANARY}\x1b[201~")
        screen = _wait(tui, lambda s: "•" * len(CANARY) in s, "masked canary bullets")
        assert CANARY not in screen
        tui.send("\x1b")
        screen = _wait(tui, lambda s: "Login cancelled" in s
                       and "Sign in to Anthropic" not in s and "Local shortcut" not in s,
                       "Login cancelled with the modal removed")
        _capture(tui, evidence, f"{name}-cancelled", screen)
        with socket.socket() as probe:
            probe.settimeout(5)
            assert probe.connect_ex(("127.0.0.1", port)) != 0, "callback port still open"
        with lock:
            observed = list(requests)
        assert all(urlsplit(row["path"]).path == "/api.json" for row in observed), observed
        raw.flush()
        for artifact in (f"{name}.screens.jsonl", f"{name}.raw.bin"):
            assert CANARY not in (evidence / artifact).read_text(encoding="utf-8"), artifact
    # The appender writes a dated file and flushes on exit: require that one was
    # written, then check every runtime log for the canary.
    logs = sorted(evidence.glob(f"{name}.runtime.log*"))
    assert any(re.fullmatch(rf"{re.escape(name)}\.runtime\.log\.\d{{4}}-\d{{2}}-\d{{2}}", log.name)
               and log.is_file() for log in logs), "dated runtime log missing"
    for log in logs:
        assert CANARY not in log.read_text(encoding="utf-8", errors="replace"), log.name
    return osc8_bytes


@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(300)
def test_sign_in_link_is_complete_for_herdr_and_screen_80x24(monkeypatch):
    parent = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())).resolve()
    parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="20-9j-", dir=parent))
    manifest = {"run_id": evidence.name, "started_at": datetime.now(timezone.utc).isoformat(),
                "evidence_dir": str(evidence), "viewport": [80, 24],
                "spawns": ["a-herdr", "b-screen"], "result": "RUNNING"}
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    original_spawn = harness.pexpect.spawn
    try:
        binary = harness._resolve_binary()[0].resolve()
        assert binary.is_file() and "debug" in binary.parts and "release" not in binary.parts, binary
        manifest.update(binary=str(binary), binary_sha256=_sha256(binary))
        (evidence / "run.json").write_text(json.dumps(manifest, indent=2) + "\n")
        cases = [
            ("a-herdr", {"TERM": "xterm-256color", "TERM_PROGRAM": "herdr", "TMUX": "", "STY": ""}),
            ("b-screen", {"TERM": "xterm-256color", "TERM_PROGRAM": "", "TMUX": "", "STY": "20-9j"}),
        ]
        for name, expected in cases:
            with tempfile.TemporaryDirectory(prefix=f"r20-9j-{name}-") as root:
                manifest[f"{name}_osc8_bytes_first_paint"] = _spawn_case(
                    name, binary, evidence, Path(root), original_spawn, monkeypatch, expected)
        assert manifest["a-herdr_osc8_bytes_first_paint"] > 0
        assert manifest["b-screen_osc8_bytes_first_paint"] == 0
        assert _sha256(binary) == manifest["binary_sha256"], "binary changed during native smoke"
        manifest["result"] = "PASS"
    except BaseException:
        manifest["result"] = "FAIL"
        (evidence / "failure.txt").write_text(traceback.format_exc())
        raise
    finally:
        manifest["completed_at"] = datetime.now(timezone.utc).isoformat()
        (evidence / "run.json").write_text(json.dumps(manifest, indent=2) + "\n")
        print(f"NATIVE_20_9J_EVIDENCE={evidence}", flush=True)
