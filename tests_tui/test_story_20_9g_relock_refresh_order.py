"""Real 80x24 relock/relogin with a held obsolete startup discovery."""
from contextlib import ExitStack
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import tempfile
import threading
import time
import traceback
from urllib.parse import urlsplit

import harness
import pexpect

from test_story_20_1c_locked_providers import PROVIDER_ENV, _NativeTUI, _plain, _sensitive_env, _sha256
from test_story_20_1e2_accounts_init import _modal

OLD_KEY = "sk-20-9g-old-synthetic-secret"
NEW_KEY = "sk-20-9g-new-synthetic-secret"
OLD_MODEL = "obsolete-only-20-9g"
NEW_MODEL = "new-only-20-9g"
REJECTION = "rejected its credential"


class ByteTUI(_NativeTUI):
    def until(self, predicate, label, timeout=60):
        deadline = time.monotonic() + timeout
        while True:
            screen = "\n".join(self._screen.display)
            if predicate(screen):
                return screen
            remaining = deadline - time.monotonic()
            assert remaining > 0, f"Timed out waiting for {label}:\n{screen}"
            try:
                data = self.child.read_nonblocking(size=65536, timeout=remaining)
            except pexpect.TIMEOUT:
                raise AssertionError(f"Timed out waiting for {label}:\n{screen}") from None
            self._stream.feed(data)
            self.screens.write(json.dumps({"screen": "\n".join(self._screen.display)}) + "\n")
            self.screens.flush()

    def wait_for_screen(self, text, timeout=60, poll_interval=0.5):
        self.until(lambda screen: text in screen, text, timeout)
        return True


def test_relock_relogin_discards_old_startup_discovery_80x24(monkeypatch):
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    parent = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())).resolve()
    parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="20-9g-", dir=parent))
    binary = harness._resolve_binary()[0].resolve()
    assert binary.is_file() and "debug" in binary.parts and "release" not in binary.parts, binary
    digest = _sha256(binary)
    requests = []
    handler_errors = []
    schedule = []
    lock = threading.Lock()
    startup = threading.Event()
    old_arrived = threading.Event()
    old_release = threading.Event()
    old_finished = threading.Event()
    old_delivered = threading.Event()
    old_startup_count = 0
    new_count = 0

    with tempfile.TemporaryDirectory(prefix="r20-9g-") as tmp, ExitStack() as stack:
        root = Path(tmp)
        workspace = root / "workspace"
        workspace.mkdir()
        home = root / "home"
        config = home / "config"
        data = home / "data"
        config.mkdir(parents=True)
        data.mkdir()

        class Stub(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def reply(self, status, payload, content_type="application/json"):
                body = payload if isinstance(payload, bytes) else json.dumps(payload).encode()
                self.send_response(status)
                self.send_header("Content-Type", content_type)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_GET(self):
                nonlocal new_count, old_startup_count
                path = urlsplit(self.path).path
                auth = self.headers.get("Authorization")
                key = "old" if auth == f"Bearer {OLD_KEY}" else "new" if auth == f"Bearer {NEW_KEY}" else "other"
                if path == "/api.json":
                    with lock:
                        requests.append({"path": path, "key": key, "phase": "catalog"})
                    return self.reply(200, {})
                if path != "/openai/v1/models":
                    with lock:
                        requests.append({"path": path, "key": key, "phase": "unexpected"})
                    return self.reply(404, {})
                with lock:
                    if key == "new":
                        new_count += 1
                        phase = "new-validation" if new_count == 1 else "new-discovery"
                    elif key == "old":
                        if startup.is_set():
                            old_startup_count += 1
                            # Startup awaits health before creating the live
                            # layer and spawning actual discovery. Only that
                            # second request is A; holding health would consume
                            # its five-second timeout before TUI input.
                            phase = ("old-health" if old_startup_count == 1 else
                                     "old-startup" if old_startup_count == 2 else "old-check")
                        else:
                            phase = "old-login"
                    else:
                        phase = "unexpected"
                    requests.append({"path": path, "key": key, "phase": phase})
                if phase == "old-startup":
                    with lock:
                        schedule.append("old-arrived")
                    old_arrived.set()
                    try:
                        if not old_release.wait(120):
                            return
                        self.reply(401, {"error": {"message": "old key rejected"}})
                        with lock:
                            schedule.append("old-401-delivered")
                        old_delivered.set()
                    finally:
                        old_finished.set()
                    return
                model = NEW_MODEL if key == "new" else OLD_MODEL
                self.reply(200, {"object": "list", "data": [{"id": model, "object": "model"}]})

            def do_POST(self):
                path = urlsplit(self.path).path
                body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
                key = "new" if self.headers.get("Authorization") == f"Bearer {NEW_KEY}" else "other"
                with lock:
                    requests.append({"path": path, "key": key, "model": body.get("model")})
                chunk = {"id": "c", "object": "chat.completion.chunk", "model": NEW_MODEL,
                         "choices": [{"index": 0, "delta": {"role": "assistant", "content": "pong-20-9g"}, "finish_reason": None}]}
                done = {"id": "c", "object": "chat.completion.chunk", "model": NEW_MODEL,
                        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}
                self.reply(200, f"data: {json.dumps(chunk)}\n\ndata: {json.dumps(done)}\n\ndata: [DONE]\n\n".encode(), "text/event-stream")

        class StubServer(ThreadingHTTPServer):
            def handle_error(self, request, client_address):
                with lock:
                    handler_errors.append(traceback.format_exc())
                super().handle_error(request, client_address)

        server = StubServer(("127.0.0.1", 0), Stub)
        server.daemon_threads = False
        thread = threading.Thread(target=server.serve_forever)
        thread.start()

        def stop_server():
            old_release.set()
            server.shutdown()
            server.server_close()  # joins all request handlers
            thread.join(timeout=5)
            assert not thread.is_alive()

        stack.callback(stop_server)
        origin = f"http://127.0.0.1:{server.server_address[1]}"
        (config / "config.toml").write_text(
            '[provider.openai]\nprovider_id = "openai"\nkind = "openai"\n'
            'model_id = "configured-fallback-20-9g"\napi_key_env = "OPENAI_API_KEY"\n'
            f'base_url = "{origin}/openai/v1"\nenabled = true\n'
            'discover_models = true\nmodel_filter = ["*"]\ncache_ttl_seconds = 0\n')
        loaded = harness._load_env()
        cleared = (PROVIDER_ENV | {name for name in loaded if _sensitive_env(name)}) - {"NO_PROXY"}
        env = {name: "" for name in cleared}
        env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "xdg-config"),
                   XDG_DATA_HOME=str(home / "xdg-data"), RUSTAIN_CONFIG_DIR=str(config),
                   RUSTAIN_DATA_DIR=str(data), RUSTAIN_MODELS_DEV_URL=origin,
                   TERM="xterm-256color", NO_PROXY="127.0.0.1,localhost", RUST_LOG="debug")
        cli_raw = stack.enter_context((evidence / "login.raw.txt").open("w", encoding="utf-8", newline=""))
        cli = pexpect.spawn(str(binary), ["auth", "login", "openai", "--method", "api-key"],
                            cwd=str(workspace), env={**loaded, **env, "RUSTAIN_LOG_PATH": str(evidence / "login.runtime.log")},
                            dimensions=(24, 80), encoding="utf-8")
        stack.callback(lambda: cli.close(force=True))
        cli.logfile_read = cli_raw
        cli.delaybeforesend = 0
        cli.expect_exact("Enter OpenAI API key: ", timeout=60)
        cli.send(OLD_KEY + "\r")
        cli.expect_exact("credentials stored successfully", timeout=60)
        cli.expect(pexpect.EOF, timeout=60)
        cli.close()
        assert cli.exitstatus == 0

        startup.set()
        runtime_log = evidence / "native.runtime.log"
        env["RUSTAIN_LOG_PATH"] = str(runtime_log)
        raw = stack.enter_context((evidence / "native.raw.txt").open("w", encoding="utf-8", newline=""))
        original_spawn = harness.pexpect.spawn

        def spawn(command, *args, **kwargs):
            assert Path(command).resolve() == binary and kwargs["dimensions"] == (24, 80)
            assert all(kwargs["env"].get(name) == "" for name in cleared)
            child = original_spawn(command, *args, **kwargs)
            child.logfile_read = raw  # includes startup bytes before start() returns
            child.delaybeforesend = 0
            return child

        monkeypatch.setattr(harness.pexpect, "spawn", spawn)
        tui = ByteTUI(binary=binary, build=False, workspace=workspace, env_overrides=env)
        tui.screens = stack.enter_context((evidence / "native.screens.jsonl").open("w", encoding="utf-8"))
        stack.callback(tui.stop)
        stack.callback(old_release.set)
        tui.start()
        assert old_arrived.wait(60), "startup discovery with old bearer did not arrive"
        tui.send("/logout")
        tui.until(lambda s: "Log out of one provider account" in s, "logout completion")
        tui.send("\r")
        tui.until(lambda s: "Log out of one provider account" not in s, "logout completion accepted")
        tui.send("\r")
        tui.until(lambda s: _modal(s, "Log out") and "OpenAI · login" in s, "logout provider")
        tui.send("\r")
        tui.until(lambda s: _modal(s, "Log out of OpenAI") and "Enter log out" in s, "last account")
        tui.send("\r")
        tui.until(lambda s: "No OpenAI credentials remain." in _plain(s), "last-row relock")
        tui.send("/login openai\r")
        tui.until(lambda s: _modal(s, "Sign in to OpenAI") and "Enter OpenAI API key:" in s, "new key prompt")
        tui.send(f"\x1b[200~{NEW_KEY}\x1b[201~")
        tui.until(lambda s: "•" * len(NEW_KEY) in s, "masked new key")
        tui.send("\r")
        tui.until(lambda s: "Discovered 1 models for OpenAI." in _plain(s), "new discovery published")
        # B's completion event is the structural publication acknowledgement.
        # Release A now, not after additional picker interactions.
        with lock:
            schedule.append("new-published")
        old_release.set()
        assert old_finished.wait(60), "old 401 handler did not finish"
        assert old_delivered.is_set(), "old 401 was not delivered to the live request"
        # Successful login opens the selector even when initiated by /login.
        tui.until(lambda s: _modal(s, "Select Model") and f"▸ {NEW_MODEL}" in s,
                  "login-opened picker on new model")
        tui.send("\r")
        tui.until(lambda s: "Select Model" not in s and NEW_MODEL in s,
                  "new provider selected before reopening picker")
        with lock:
            phases = [r["phase"] for r in requests if "phase" in r]
        assert "new-validation" in phases and "new-discovery" in phases, phases
        tui.send("/models")
        tui.until(lambda s: "Open the model picker" in s, "models completion")
        tui.send("\r")
        tui.until(lambda s: "Open the model picker" not in s, "models completion accepted")
        tui.send("\r")
        screen = tui.until(lambda s: _modal(s, "Select Model") and NEW_MODEL in s, "new model picker")
        assert OLD_MODEL not in screen and REJECTION not in _plain(screen), screen
        assert f"▸ {NEW_MODEL}" in screen, screen
        (evidence / "picker.screen.txt").write_text(screen + "\n", encoding="utf-8")
        tui.send("\r")
        tui.until(lambda s: "Select Model" not in s and NEW_MODEL in s, "new model selected")
        tui.send("hello\r")
        tui.until(lambda s: "pong-20-9g" in s, "new-key response")
        tui.send("\x11")
        tui.child.expect(pexpect.EOF, timeout=30)
        with lock:
            observed = list(requests)
        chats = [r for r in observed if r["path"] == "/openai/v1/chat/completions"]
        assert chats and chats[-1]["key"] == "new" and chats[-1]["model"] == NEW_MODEL, chats
        assert all(r["path"] in {"/api.json", "/openai/v1/models", "/openai/v1/chat/completions"} for r in observed), observed
        (evidence / "requests.json").write_text(json.dumps(observed, indent=2) + "\n")
        raw.flush()
        cli_raw.flush()
        for path in (evidence / "native.raw.txt", evidence / "login.raw.txt", evidence / "native.screens.jsonl"):
            content = path.read_text(encoding="utf-8")
            assert OLD_KEY not in content and NEW_KEY not in content and REJECTION not in content, path
    assert not handler_errors, f"loopback handler errors: {handler_errors}"
    assert schedule == ["old-arrived", "new-published", "old-401-delivered"], schedule
    logs = sorted(evidence.glob("native.runtime.log.*"))
    assert logs, f"dated runtime log missing: {runtime_log}.*"
    for path in logs:
        content = path.read_text(encoding="utf-8", errors="replace")
        assert OLD_KEY not in content and NEW_KEY not in content and REJECTION not in content, path
    login_logs = sorted(evidence.glob("login.runtime.log.*"))
    assert login_logs, "dated CLI login runtime log missing"
    for path in login_logs:
        content = path.read_text(encoding="utf-8", errors="replace")
        assert OLD_KEY not in content and NEW_KEY not in content and REJECTION not in content, path
    assert _sha256(binary) == digest, "binary changed during smoke"
    (evidence / "run.json").write_text(json.dumps({"result": "PASS", "binary": str(binary), "binary_sha256": digest,
        "viewport": [80, 24], "server_handler_errors": handler_errors,
        "schedule": schedule,
        "completed_at": datetime.now(timezone.utc).isoformat()}, indent=2) + "\n")
    print(f"NATIVE_20_9G_EVIDENCE={evidence}", flush=True)
