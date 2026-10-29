"""`rustain init` sign-in and TUI `/accounts` / `/logout` on a real debug binary
at 80x24, loopback only.

Phase A: `rustain init` in a PTY keeps the existing configuration, answers the
policy prompts, offers sign-in, lists the providers the configuration enables,
signs in to OpenAI through the CLI login (masked rpassword prompt, validation,
discovery line) and lists again; Enter finishes with exit 0. Phase D: the TUI
over the same world shows the account under `/accounts openai`, removes it
through `/logout` (no confirmation), posts the locked warning, and the model
picker shows OpenAI locked again.
"""
from contextlib import ExitStack
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import tempfile
import threading
import traceback
from urllib.parse import urlsplit

import harness
import pexpect
import pyte
import pytest

from test_story_20_1c_locked_providers import (
    PROVIDER_ENV, _NativeTUI, _capture, _plain, _sensitive_env, _sha256, _wait,
)

KEY = "sk-native-openai-20-1e2-SECRET"


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


class _Init:
    """`rustain init` in a PTY: ordered expects on the received bytes, rendered
    through pyte for the captures."""

    def __init__(self, child):
        self.child = child
        self._screen = pyte.Screen(80, 24)
        self._stream = pyte.Stream(self._screen)

    def expect(self, text, timeout=60):
        self.child.expect_exact(text, timeout=timeout)
        self._stream.feed(self.child.before + self.child.after)
        return self.child.before + self.child.after

    def screen(self):
        return "\n".join(self._screen.display)


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
    requests = []
    lock = threading.Lock()

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
            if path == "/openai/v1/models":
                self._record(200)
                return self._reply(200, json.dumps({"object": "list", "data": [
                    {"id": "native-one", "object": "model"}]}).encode())
            if path == "/api.json":
                self._record(200)
                return self._reply(200, b"{}")
            self._record(404)
            self._reply(404, b'{"error":"unexpected route"}')

        def do_POST(self):
            self._record(404)
            self._reply(404, b'{"error":"unexpected route"}')

    with ExitStack() as stack:
        server = ThreadingHTTPServer(("127.0.0.1", 0), Stub)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()

        def stop_server():
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)

        stack.callback(stop_server)
        origin = f"http://127.0.0.1:{server.server_address[1]}"
        (config / "config.toml").write_text(
            '[provider.openai]\nprovider_id = "openai"\nkind = "openai"\n'
            'model_id = "native-one"\napi_key_env = "OPENAI_API_KEY"\n'
            f'base_url = "{origin}/openai/v1"\nenabled = true\n'
            'discover_models = true\nmodel_filter = ["*"]\n')
        loaded = harness._load_env()
        cleared = (PROVIDER_ENV | {name for name in loaded if _sensitive_env(name)}) - {"NO_PROXY"}
        env = {name: "" for name in cleared}
        env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "xdg-config"),
                   XDG_DATA_HOME=str(home / "xdg-data"), RUSTAIN_CONFIG_DIR=str(config),
                   RUSTAIN_DATA_DIR=str(data), RUSTAIN_MODELS_DEV_URL=origin,
                   TERM="xterm-256color", NO_PROXY="127.0.0.1,localhost")

        # ── Phase A: rustain init → sign in → list again → finish ─────────
        init_env = {**loaded, **env, "RUST_LOG": "debug",
                    "RUSTAIN_LOG_PATH": str(evidence / "init.runtime.log")}
        assert Path(binary).resolve() == binary and "debug" in binary.parts
        child = pexpect.spawn(str(binary), ["init"], cwd=str(workspace), env=init_env,
                              dimensions=(24, 80), encoding="utf-8")
        child.delaybeforesend = 0
        stack.callback(lambda: child.close(force=True))
        init_raw = stack.enter_context((evidence / "init.raw.bin").open("w", encoding="utf-8", newline=""))
        # Received bytes only: the key typed into the PTY is never teed.
        child.logfile_read = init_raw
        init = _Init(child)
        init.expect("Configuration already exists. Overwrite? [y/n] ")
        child.send("n\r")
        init.expect("Selection [1]: ")
        child.send("\r")
        init.expect("Selection [1]: ")
        child.send("\r")
        init.expect("rustain init completed!")
        init.expect("Sign in to a model provider now? [y/n] ")
        (evidence / "a1-offer.screen.txt").write_text(init.screen() + "\n")
        child.send("y\r")
        listed = init.expect("Enter to finish]: ")
        number = re.search(r"(\d+)\. OpenAI — not signed in", listed)
        assert number, listed
        (evidence / "a2-list.screen.txt").write_text(init.screen() + "\n")
        child.send(f"{number.group(1)}\r")
        init.expect("Enter OpenAI API key: ")
        (evidence / "a3-masked-prompt.screen.txt").write_text(init.screen() + "\n")
        child.send(f"{KEY}\r")
        init.expect("✓ OpenAI credentials stored successfully.")
        init.expect("Credentials saved to ")
        init.expect("Discovered 1 models for OpenAI.")
        again = init.expect("Enter to finish]: ")
        assert re.search(rf"{number.group(1)}\. OpenAI — login", again), again
        (evidence / "a4-list-again.screen.txt").write_text(init.screen() + "\n")
        child.send("\r")
        child.expect(pexpect.EOF, timeout=60)
        child.close()
        assert child.exitstatus == 0, child.exitstatus
        with lock:
            validated = [row for row in requests if row["path"] == "/openai/v1/models"]
        assert validated and all(row["key_received"] for row in validated), validated

        # ── Phase D: /accounts, /logout, relock (same world) ─────────────
        runtime_log = evidence / "native.runtime.log"
        env["RUSTAIN_LOG_PATH"] = str(runtime_log)
        original_spawn = harness.pexpect.spawn

        def safe_spawn(command, *args, **kwargs):
            actual = kwargs["env"]
            assert Path(command).resolve() == binary and "debug" in binary.parts and "release" not in binary.parts
            assert kwargs["dimensions"] == (24, 80)
            assert all(actual.get(name) == "" for name in cleared)
            assert actual["RUSTAIN_DATA_DIR"] == str(data)
            return original_spawn(command, *args, **kwargs)

        monkeypatch.setattr(harness.pexpect, "spawn", safe_spawn)
        tui = _NativeTUI(binary=binary, build=False, workspace=workspace, env_overrides=env)
        tui.screens = stack.enter_context((evidence / "native.screens.jsonl").open("w"))
        raw = stack.enter_context((evidence / "native.raw.bin").open("w", encoding="utf-8", newline=""))
        stack.callback(tui.stop)
        tui.start()
        tui.child.delaybeforesend = 0
        tui.child.logfile_read = raw

        tui.send("/accounts openai")
        tui.send("\r")
        screen = _wait(tui, lambda s: _modal(s, "OpenAI accounts")
                       and "#1  API key #1  api key  active" in s
                       and "Enter make active · Esc close" in s, "the account step")
        _capture(tui, evidence, "d1-accounts", screen)
        tui.send("\x1b")
        _wait(tui, lambda s: "OpenAI accounts" not in s, "the account step closed")
        tui.send("/logout")
        # The slash-command popup takes the first Enter (it completes the name).
        _wait(tui, lambda s: "Log out of one provider account" in s, "the /logout completion")
        tui.send("\r")
        _wait(tui, lambda s: "Log out of one provider account" not in s, "completion accepted")
        tui.send("\r")
        screen = _wait(tui, lambda s: _modal(s, "Log out") and "OpenAI · login" in s,
                       "the logout provider step")
        _capture(tui, evidence, "d2-logout-providers", screen)
        tui.send("\r")
        screen = _wait(tui, lambda s: _modal(s, "Log out of OpenAI")
                       and "OpenAI (openai): serving API key #1" in _plain(s)
                       and "Enter log out · Esc back" in s, "the logout account step")
        _capture(tui, evidence, "d3-logout-account", screen)
        tui.send("\r")
        screen = _wait(tui, lambda s: "Logged out of OpenAI account API key #1." in _plain(s)
                       and "No OpenAI credentials remain." in _plain(s), "logout result")
        # The locked warning is a Warning notice: the default Focus density
        # (`[F]`) queues it rather than painting it (K3 proves it is posted);
        # the relock itself is checked in the model picker below.
        _capture(tui, evidence, "d4-logged-out", screen)
        tui.send("\x18")
        tui.send("m")
        screen = _wait(tui, lambda s: _modal(s, "Select Model") and "OpenAI" in s
                       and "not signed in" in s, "OpenAI locked in the model picker")
        _capture(tui, evidence, "d5-locked", screen)
        tui.send("\x1b")
        _wait(tui, lambda s: "Select Model" not in s, "picker closed")
        tui.send("\x11")
        tui.child.expect(pexpect.EOF, timeout=30)

        with lock:
            observed = list(requests)
        (evidence / "requests.json").write_text(json.dumps(observed, indent=2) + "\n")
        assert all(urlsplit(row["path"]).path in {"/openai/v1/models", "/api.json"}
                   for row in observed), observed
        raw.flush()
        init_raw.flush()
        for name in ("native.screens.jsonl", "native.raw.bin", "init.raw.bin"):
            assert KEY not in (evidence / name).read_text(encoding="utf-8"), name
        for capture in evidence.glob("*.screen.txt"):
            assert KEY not in capture.read_text(encoding="utf-8"), capture.name
    # The appender writes dated files and flushes on exit: check every runtime
    # log once both processes stopped, and require that each wrote one.
    for prefix in ("init.runtime.log", "native.runtime.log"):
        logs = sorted(evidence.glob(f"{prefix}*"))
        assert logs, f"{prefix} missing"
        for log in logs:
            assert KEY not in log.read_text(encoding="utf-8", errors="replace"), log.name


@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(240)
def test_init_signin_and_tui_logout_80x24(monkeypatch):
    parent = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())).resolve()
    parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="20-1e2-", dir=parent))
    manifest = {"run_id": evidence.name, "started_at": datetime.now(timezone.utc).isoformat(),
                "evidence_dir": str(evidence), "viewport": [80, 24], "phases": ["A", "D"],
                "result": "RUNNING"}
    try:
        binary = harness._resolve_binary()[0].resolve()
        assert binary.is_file() and "debug" in binary.parts and "release" not in binary.parts, binary
        manifest.update(binary=str(binary), binary_sha256=_sha256(binary))
        (evidence / "run.json").write_text(json.dumps(manifest, indent=2) + "\n")
        with tempfile.TemporaryDirectory(prefix="r20-1e2-") as root:
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
        print(f"NATIVE_20_1E2_EVIDENCE={evidence}", flush=True)
