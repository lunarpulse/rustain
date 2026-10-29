"""Story 20-1d4a K8: a multi-response Gemini tool turn survives a TUI reload.

Real debug-native 80x24 TUI against a loopback Cloud Code Assist stub. Run A
drives one tool turn across three provider responses (signed thought, signed
text, a signed image call and an unsigned echo call, then a signed echo call,
then the answer); Run B reopens the same session with ``--session <id>`` and
its next request must replay Run A's in-turn history byte for byte.

The OAuth row can only reach the stub through the instrumented
``RUSTAIN_TEST_AUTH_ORIGIN`` rebasing: on a binary built without
``test-instrumentation`` the login probe sees ``/launch`` redirect outside the
stub, kills the login and skips, having contacted nothing remote.
"""
from base64 import b64decode, b64encode
from contextlib import ExitStack
from datetime import datetime, timezone
import hashlib
from html import escape
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import queue
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import traceback
from urllib.parse import parse_qsl, urlencode, urlsplit

from conftest import MCP_FIXTURE, _write_mcp_json, _write_test_mcp_profile
from fixtures.mcp_fixture import PNG_1X1_TRANSPARENT_B64
import harness
import pexpect
import pytest

PROVIDER = "google-gemini-cli"
MODEL = "gemini-3.1-pro-preview"
PROJECT = "gemini-fixture-project"
EMAIL = "operator@example.test"
ACCESS = "GEMINI-ACCESS-CANARY"
REFRESH = "GEMINI-REFRESH-CANARY"
CODE = "GEMINI-CODE-CANARY"
NOW_MS = 1_800_000_000_000
STREAM = "/v1internal:streamGenerateContent"
LOAD = "/v1internal:loadCodeAssist"
ONBOARD = "/v1internal:onboardUser"
OPERATION = "/v1internal/operations/fixture-op"
TOKEN = "/token"
USERINFO = "/oauth2/v1/userinfo"
# Distinctive valid Base64 thought signatures (all start with "U0lH" = "SIG").
SIGNATURES = [b64encode(f"SIG{n}:k8-tool-turn-reload".encode()).decode() for n in range(4)]
THOUGHT = "K8 weighing which tools to call"
VISIBLE = "K8 fetching the image and an echo."
ECHO_FIRST = "k8-first-echo"
ECHO_SECOND = "k8-second-echo"
FINAL = "K8 final answer after two tool rounds."
PROMPT_A = "k8 run the image and echo tools"
PROMPT_B = "k8 continue after reload"
REPLY_B = "K8 reload reply received."
AUX_TITLE = "K8 title"
MCP_SERVER = "echo"
ALLOWED = ["Read", "Glob", "Grep", f"mcp__{MCP_SERVER}__echo", f"mcp__{MCP_SERVER}__image_content"]
PROVIDER_ENV = {
    "ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "OPENAI_API_KEY",
    "OPENAI_CODEX_OAUTH_TOKEN",
    "OPENROUTER_API_KEY", "GOOGLE_API_KEY", "GEMINI_API_KEY", "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_CLOUD_PROJECT", "GOOGLE_CLOUD_PROJECT_ID",
    "DEEPSEEK_API_KEY", "MOONSHOT_API_KEY", "KIMI_API_KEY", "XAI_API_KEY",
    "GROQ_API_KEY", "MISTRAL_API_KEY", "ZAI_API_KEY", "VOYAGE_API_KEY",
    "DEEPINFRA_API_KEY", "TOGETHER_API_KEY", "RUSTAIN_EMBEDDING_API_KEY",
    "ANTHROPIC_BASE_URL", "OPENAI_BASE_URL", "OPENROUTER_BASE_URL",
    "GOOGLE_BASE_URL", "GEMINI_BASE_URL", "DEEPSEEK_BASE_URL", "MOONSHOT_BASE_URL",
    "OLLAMA_HOST", "OLLAMA_BASE_URL", "RUSTAIN_TEST_AUTH_ORIGIN",
}


def _sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def _sensitive_env(name):
    return (name in PROVIDER_ENV or bool(re.search(
        r"(?:API_KEY|AUTH_TOKEN|ACCESS_TOKEN|BASE_URL|ENDPOINT|PROXY)$", name))
        or name.startswith(("RUSTAIN_TEST_", "RUSTAIN_SCENE_")))


class _NativeTUI(harness.RustainTUI):
    """Shared native spawn, with PTY-driven waits instead of legacy sleeps."""

    def _sync_screen(self):
        try:
            data = self.child.read_nonblocking(size=65536, timeout=0.2)
        except pexpect.TIMEOUT:
            assert self.child.isalive(), "native TUI exited"
            return
        except pexpect.EOF:
            pytest.fail("native TUI exited before the expected screen")
        self._stream.feed(data)
        self.screens.write(json.dumps({"screen": "\n".join(self._screen.display)}) + "\n")
        self.screens.flush()

    def wait_for_screen(self, text, timeout=60, poll_interval=0.5):
        _wait(self, lambda screen: text in screen, repr(text), timeout)
        return True  # Never enter the shared harness's timeout-to-sleep fallback.

    def quit(self, timeout=30):
        """The normal quit path (Ctrl+Q, any focus); returns the exit status."""
        self.child.sendcontrol("q")
        while True:
            try:
                data = self.child.read_nonblocking(size=65536, timeout=timeout)
            except pexpect.EOF:
                break
            self._stream.feed(data)
            self.screens.write(json.dumps({"screen": "\n".join(self._screen.display)}) + "\n")
        self.child.close()
        return self.child.exitstatus, self.child.signalstatus

    def stop(self):
        if self._child is not None:
            if self.child.isalive():
                self.child.sendcontrol("c")
                try:
                    self.child.expect(pexpect.EOF, timeout=5)
                except pexpect.TIMEOUT:
                    self.child.terminate(force=True)
            self.child.close(force=True)


def _wait(tui, predicate, label, timeout=60):
    deadline = time.monotonic() + timeout
    while True:
        screen = tui.get_screen_text()
        if predicate(screen):
            return screen
        assert time.monotonic() < deadline, f"Timed out waiting for {label}:\n{screen}"


def _plain(screen):
    return " ".join(screen.replace("│", " ").replace("┆", " ").split())


def _capture(tui, evidence, name, screen):
    """Persist the actual full PTY grid, not a reconstructed UI mock."""
    rows = list(tui._screen.display)
    assert len(rows) == 24 and tui._screen.columns == 80
    assert screen == "\n".join(rows)
    (evidence / f"{name}.screen.txt").write_text(screen + "\n")
    lines = [f'<text x="8" y="{22 + y * 20}" xml:space="preserve">{escape(row)}</text>'
             for y, row in enumerate(rows)]
    (evidence / f"{name}.svg").write_text(
        '<svg xmlns="http://www.w3.org/2000/svg" width="976" height="496" viewBox="0 0 976 496">'
        '<rect width="976" height="496" fill="#101418"/>'
        '<g fill="#e8edf2" font-family="monospace" font-size="20">'
        + "".join(lines) + '</g></svg>\n')


def _loopback(url, port=None):
    parsed = urlsplit(url)
    assert parsed.scheme == "http" and parsed.hostname == "127.0.0.1", url
    assert not parsed.username and not parsed.password, url
    if port is not None:
        assert parsed.port == port, url
    return parsed


def _get_no_redirect(url):
    """One loopback GET; redirects are reported, never followed."""
    parsed = _loopback(url)
    connection = http.client.HTTPConnection(parsed.hostname, parsed.port, timeout=10)
    try:
        target = parsed.path + (f"?{parsed.query}" if parsed.query else "")
        connection.request("GET", target)
        response = connection.getresponse()
        response.read()
        return response.status, response.getheader("Location")
    finally:
        connection.close()


def _stored(workspace, session_id):
    """Every text file of one session in the workspace session store."""
    store = workspace / ".claude/sessions"
    return "".join(path.read_text(errors="replace") for path in sorted(store.rglob("*"))
                   if path.is_file() and session_id in str(path.relative_to(store)))


def _sse(parts_frames):
    frames = [{"response": {"candidates": [{"content": {"role": "model", "parts": parts}}]}}
              for parts in parts_frames]
    frames.append({"response": {
        "candidates": [{"content": {"role": "model", "parts": []}, "finishReason": "STOP"}],
        "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 3,
                          "thoughtsTokenCount": 2, "totalTokenCount": 15}}})
    return "".join(f"data: {json.dumps(frame)}\n\n" for frame in frames).encode()


def _classify(body, names):
    """Which response a stream POST asks for, from its conversation shape.
    Tool-less side requests (the session-title generator) are auxiliary."""
    if "tools" not in body["request"]:
        return None
    contents = body["request"]["contents"]
    models = sum(1 for content in contents if content["role"] == "model")
    last = contents[-1]
    texts = " ".join(part.get("text", "") for part in last["parts"])
    responds = any("functionResponse" in part for part in last["parts"])
    if last["role"] == "user" and PROMPT_B in texts:
        return 4
    if last["role"] == "user" and PROMPT_A in texts and models == 0:
        return 1
    if responds and models == 1:
        return 2
    if responds and models == 2:
        return 3
    return None


def _login(binary, workspace, env, origin, held_port, evidence):
    """`auth login google-gemini-cli` driven like a browser. Returns None on
    success, or the skip reason when the binary lacks test-instrumentation."""
    child = subprocess.Popen([str(binary), "auth", "login", PROVIDER], cwd=workspace, env=env,
                             stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                             stderr=subprocess.PIPE, text=True)
    lines = queue.Queue()
    transcript = []

    def pump():
        for line in child.stderr:
            lines.put(line)
        lines.put(None)

    reader = threading.Thread(target=pump, daemon=True)
    reader.start()
    try:
        authorize = launch = None
        while launch is None:
            line = lines.get(timeout=30)
            assert line is not None, "login exited before printing its URLs:\n" + "".join(transcript)
            transcript.append(line)
            assert sum(map(len, transcript)) <= 65536, "unbounded login stderr"
            text = line.strip()
            if text.startswith("http"):
                authorize = text
            if text.startswith("Local browser launch: "):
                launch = text[len("Local browser launch: "):]
        assert authorize is not None, "authorize URL must precede the launch URL"
        # The launch URL is the binary's own loopback listener: the probe target.
        _loopback(launch)
        assert urlsplit(launch).path == "/launch", launch
        status, location = _get_no_redirect(launch)
        location_origin = (f"{urlsplit(location).scheme}://{urlsplit(location).netloc}"
                           if location else None)
        probe = {"launch_url": launch, "launch_status": status, "launch_location": location,
                 "stub_origin": origin, "authorize_url_origin_matches_stub":
                 authorize.startswith(origin + "/"), "redirect_followed": False}
        if status != 302 or location_origin != origin:
            child.kill()
            child.wait(timeout=30)
            probe["result"] = "SKIP: /launch redirects outside the loopback stub"
            return probe, ("binary lacks the test-instrumentation feature: /launch redirected to "
                           f"{location_origin!r}, not the loopback stub {origin!r}; build with "
                           "`cargo build --features test-instrumentation --bin rustain`")
        assert location == authorize, (location, authorize)
        query = dict(parse_qsl(urlsplit(authorize).query))
        assert re.fullmatch(r"[0-9a-f]{32}", query["state"]), query
        preferred = f"Preferred port {held_port} unavailable, using port "
        actual = int(next(line[len(preferred):] for line in "".join(transcript).splitlines()
                          if line.startswith(preferred)).strip())
        assert actual != held_port
        callback = query["redirect_uri"]
        _loopback(callback, actual)
        _loopback(launch, actual)
        assert urlsplit(callback).path == "/oauth2callback", callback
        callback += "?" + urlencode({"code": CODE, "state": query["state"]})
        status, _ = _get_no_redirect(callback)
        assert 200 <= status < 300, status
        stdout, _ = child.communicate(timeout=30)
        reader.join(timeout=30)
        while True:
            line = lines.get(timeout=30)
            if line is None:
                break
            transcript.append(line)
        assert child.returncode == 0, "".join(transcript)
        assert stdout.strip() == f"Logged in to Google Cloud Code Assist (Gemini CLI) as {EMAIL}", stdout
        probe.update(result="instrumented: completed the loopback callback", held_port=held_port,
                     fallback_port=actual, callback_url=callback.replace(CODE, "[code]"))
        return probe, None
    finally:
        if child.poll() is None:
            child.kill()
            child.wait(timeout=30)
        text = "".join(transcript)
        for secret in (ACCESS, REFRESH, CODE):
            assert secret not in text, "fixture credential leaked into login stderr"
        (evidence / "login.stderr").write_text(text)


def _exercise(binary, evidence, root, monkeypatch):
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    workspace = root / "workspace"
    workspace.mkdir()
    home = root / "home"
    config = home / "config"
    data = home / "data"
    config.mkdir(parents=True)
    data.mkdir()
    requests = []
    posts = {}
    names = {}
    duplicates = []
    lock = threading.Lock()
    arrived = threading.Condition(lock)

    def ok(body):
        return 200, "application/json", json.dumps(body).encode()

    def respond_stream(index):
        image, echo = names["image"], names["echo"]
        if index == 1:
            return _sse([
                [{"thought": True, "text": THOUGHT, "thoughtSignature": SIGNATURES[0]}],
                [{"text": VISIBLE}],
                [{"text": "", "thoughtSignature": SIGNATURES[1]}],
                [{"functionCall": {"id": "k8-call-image", "name": image, "args": {}},
                  "thoughtSignature": SIGNATURES[2]}],
                [{"functionCall": {"id": "k8-call-echo-1", "name": echo,
                                   "args": {"message": ECHO_FIRST}}}],
            ])
        if index == 2:
            return _sse([[{"functionCall": {"id": "k8-call-echo-2", "name": echo,
                                            "args": {"message": ECHO_SECOND}},
                           "thoughtSignature": SIGNATURES[3]}]])
        return _sse([[{"text": FINAL if index == 3 else REPLY_B}]])

    class Stub(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_args):
            pass

        def _serve(self, method):
            length = int(self.headers.get("Content-Length") or 0)
            raw = self.rfile.read(length) if length else b""
            path = urlsplit(self.path).path
            # Evidence keeps the form shape but never the embedded client secret.
            row = {"method": method, "path": self.path,
                   "authorization": self.headers.get("Authorization"),
                   "body": re.sub(r"client_secret=[^&]*", "client_secret=[client-secret]",
                                  raw.decode("utf-8", "replace"))}
            index = None
            if method == "POST" and path == TOKEN:
                status, kind, body = ok({"access_token": ACCESS, "expires_in": 3600,
                                         "token_type": "Bearer", "refresh_token": REFRESH})
            elif method == "GET" and path == USERINFO:
                status, kind, body = ok({"email": EMAIL, "id": "1"})
            elif method == "POST" and path == LOAD:
                status, kind, body = ok({"allowedTiers": [{"id": "free-tier", "isDefault": True}]})
            elif method == "POST" and path == ONBOARD:
                status, kind, body = ok({"name": "operations/fixture-op", "done": False})
            elif method == "GET" and path == OPERATION:
                status, kind, body = ok({"done": True, "response": {
                    "cloudaicompanionProject": {"id": PROJECT}}})
            elif method == "POST" and path == "/v1internal:fetchAvailableModels":
                status, kind, body = 403, "application/json", b'{"error":"unavailable"}'
            elif method == "POST" and path == "/v1internal:retrieveUserQuota":
                status, kind, body = ok({"buckets": []})
            elif method == "GET" and path == "/api.json":
                status, kind, body = ok({})
            elif method == "POST" and path == STREAM:
                parsed = json.loads(raw)
                with lock:
                    declared = [d["name"] for tool in parsed["request"].get("tools", [])
                                for d in tool.get("functionDeclarations", [])]
                    if declared and not names:
                        row["declared"] = declared
                        for key, suffix in (("image", "image_content"), ("echo", "echo")):
                            match = [name for name in declared
                                     if name.endswith(suffix) and "mcp" in name]
                            if len(match) == 1:
                                names[key] = match[0]
                    index = _classify(parsed, names) if len(names) == 2 else None
                    row["response_index"] = index
                if index is None:
                    status, kind, body = 200, "text/event-stream", _sse([[{"text": AUX_TITLE}]])
                else:
                    status, kind, body = 200, "text/event-stream", respond_stream(index)
            else:
                status, kind, body = 404, "application/json", b'{"error":"unexpected route"}'
            row["status"] = status
            self.send_response(status)
            self.send_header("Content-Type", kind)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            self.wfile.flush()
            with arrived:
                requests.append(row)
                if index in posts:
                    duplicates.append(index)
                elif index is not None:
                    posts[index] = {"raw": raw.decode(), "body": json.loads(raw),
                                    "authorization": row["authorization"]}
                arrived.notify_all()

        def do_GET(self):
            self._serve("GET")

        def do_POST(self):
            self._serve("POST")

    def await_post(index, timeout=90):
        with arrived:
            assert arrived.wait_for(lambda: index in posts, timeout=timeout), \
                f"stub request {index} never arrived: {[r['path'] for r in requests]}"
            return posts[index]

    with ExitStack() as stack:
        server = ThreadingHTTPServer(("127.0.0.1", 0), Stub)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()

        def stop_server():
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)
            assert not thread.is_alive(), "loopback server failed to stop"

        stack.callback(stop_server)
        origin = f"http://127.0.0.1:{server.server_address[1]}"
        # Held for the whole run: the binary must fall back from the port it is given.
        held = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        stack.callback(held.close)
        held.bind(("127.0.0.1", 0))
        held.listen(1)
        held_port = held.getsockname()[1]

        (config / "config.toml").write_text(
            f'model = "{MODEL}"\n\n[provider.gemini]\nkind = "{PROVIDER}"\n'
            f'provider_id = "{PROVIDER}"\nmodel_id = "{MODEL}"\napi_key_env = ""\n'
            f'base_url = "{origin}"\nenabled = true\ndiscover_models = true\n')
        _write_test_mcp_profile(config, profile_name="tests-mcp")
        _write_mcp_json(workspace, server_name=MCP_SERVER)
        # Project permissions: both MCP tools pre-allowed (MCP tools prompt by default).
        (workspace / ".rustain").mkdir()
        (workspace / ".rustain/config.toml").write_text(
            f"[permissions]\nalways_tools = {json.dumps(ALLOWED)}\n")

        loaded = harness._load_env()
        cleared = (PROVIDER_ENV | {name for name in loaded if _sensitive_env(name)}) - {"NO_PROXY"}
        env = {name: "" for name in cleared}
        auth_env = {
            "RUSTAIN_TEST_AUTH_ORIGIN": origin,
            "RUSTAIN_TEST_AUTH_CALLBACK_PORT": str(held_port),
            "RUSTAIN_TEST_AUTH_SLEEP_SCALE": "0",
            "RUSTAIN_TEST_GEMINI_CLI_RETRY_SCALE": "0",
            "RUSTAIN_TEST_NOW_MS": str(NOW_MS),
        }
        env.update(auth_env)
        env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "xdg-config"),
                   XDG_DATA_HOME=str(home / "xdg-data"), XDG_CACHE_HOME=str(home / "xdg-cache"),
                   RUSTAIN_CONFIG_DIR=str(config), RUSTAIN_DATA_DIR=str(data),
                   RUSTAIN_LOG_PATH=str(evidence / "native.runtime.log"),
                   RUSTAIN_MODELS_DEV_URL=origin, RUSTAIN_PROFILE="tests-mcp",
                   TERM="xterm-256color", NO_PROXY="*", no_proxy="*")
        command_env = {**loaded, **env}
        safety = {"binary": str(binary), "instrumentation_required": True,
                  "stub_origin": origin, "held_callback_port": held_port,
                  "cleared_environment_names": sorted(cleared),
                  "test_auth_environment": auth_env}

        def write_safety():
            (evidence / "network-safety.json").write_text(json.dumps(safety, indent=2) + "\n")

        # Instrumentation probe before any inference.
        probe, skip = _login(binary, workspace, command_env, origin, held_port, evidence)
        safety["instrumentation_probe"] = probe
        write_safety()
        if skip is not None:
            with lock:
                assert all(row["path"] == "/api.json" for row in requests), requests
            pytest.skip(skip)
        with lock:
            login_paths = [urlsplit(r["path"]).path for r in requests if r["path"] != "/api.json"]
        assert login_paths == [TOKEN, USERINFO, LOAD, ONBOARD, OPERATION,
                               "/v1internal:fetchAvailableModels", "/v1internal:fetchAvailableModels",
                               "/v1internal:retrieveUserQuota"], login_paths
        auth_file = (data / "auth.json").read_text()
        assert CODE not in auth_file and PROVIDER in auth_file

        # Prove the effective endpoint, not merely a value in a file it might ignore.
        shown = subprocess.run([str(binary), "config", "show", "--json"], cwd=workspace,
                               env=command_env, capture_output=True, text=True, timeout=30,
                               check=False)
        assert shown.returncode == 0, shown.stderr
        effective = json.loads(shown.stdout)["provider"]
        assert effective["gemini"]["base_url"] == origin, effective
        assert effective["gemini"]["kind"] == PROVIDER, effective
        (evidence / "effective-config.json").write_text(shown.stdout)

        original_spawn = harness.pexpect.spawn
        spawns = []

        def safe_spawn(command, *args, **kwargs):
            # This guard runs after the harness merges its .env, before native code runs.
            actual = kwargs["env"]
            assert Path(command).resolve() == binary and "debug" in binary.parts and "release" not in binary.parts
            assert Path(kwargs["cwd"]) == workspace
            assert kwargs["dimensions"] == (24, 80)
            assert all(actual.get(name) == "" for name in cleared - set(auth_env))
            assert all(actual[name] == value for name, value in auth_env.items())
            assert actual["RUSTAIN_CONFIG_DIR"] == str(config)
            assert actual["RUSTAIN_DATA_DIR"] == str(data)
            assert actual["HOME"] == str(home)
            assert actual["RUSTAIN_PROFILE"] == "tests-mcp"
            for url in (actual["RUSTAIN_MODELS_DEV_URL"], actual["RUSTAIN_TEST_AUTH_ORIGIN"]):
                _loopback(url, server.server_address[1])
            assert "[provider." not in (workspace / ".rustain/config.toml").read_text()
            spawns.append({"args": kwargs.get("args"), "cwd": kwargs["cwd"],
                           "dimensions": kwargs["dimensions"]})
            kwargs["logfile"] = terminal
            return original_spawn(command, *args, **kwargs)

        monkeypatch.setattr(harness.pexpect, "spawn", safe_spawn)
        screens = stack.enter_context((evidence / "native.screens.jsonl").open("w"))
        terminal = stack.enter_context((evidence / "native.terminal.txt").open("w"))
        frames = []

        def frame(tui, name, screen):
            _capture(tui, evidence, name, screen)
            frames.append(screen)

        def submit(tui, prompt):
            tui.send(prompt)
            _wait(tui, lambda value: prompt in value, f"prompt {prompt!r} typed into the input")
            tui.send("\r")

        # ── Run A ─────────────────────────────────────────────────────────────
        tui = _NativeTUI(binary=binary, build=False, workspace=workspace,
                         allowed_tools=ALLOWED, env_overrides=env)
        tui.screens = screens
        stack.callback(tui.stop)
        tui.start()
        tui.child.delaybeforesend = 0
        screen = _wait(tui, lambda value: "Ready" in value, "Run A ready")
        frame(tui, "01-run-a-ready", screen)
        submit(tui, PROMPT_A)
        await_post(1)
        await_post(2)
        await_post(3)
        # A three-tool turn renders folded by default (chat_pane default_collapse):
        # wait for the finished fold, move focus to chat (the input cursor hides),
        # then expand all turns (`zR`) to put the final text on screen.
        # The status bar can truncate "Ready" at 80 columns once a session title
        # exists, so the turn counts as idle once its final text is persisted.
        screen = _wait(tui, lambda value: "3 tools" in value and VISIBLE[:20] in value
                       and FINAL in _stored(workspace, ""), "Run A turn finished (folded)",
                       timeout=90)
        frame(tui, "02a-run-a-folded", screen)
        tui.send("\x1b")
        _wait(tui, lambda _value: tui._screen.cursor.hidden, "chat focus (input cursor hidden)")
        tui.send("zR")
        screen = _wait(tui, lambda value: FINAL in _plain(value),
                       "Run A final text after three provider responses")
        frame(tui, "02-run-a-final", screen)
        status, signal = tui.quit()
        assert (status, signal) == (0, None), (status, signal)
        spawns[-1]["exit_status"] = status

        sessions = [path for path in (workspace / ".claude/sessions").iterdir()
                    if path.is_dir() or path.name.endswith(".meta.json")]
        ids = sorted({path.name[:-len(".meta.json")] if path.name.endswith(".meta.json")
                      else path.name for path in sessions})
        assert len(ids) == 1, ids
        session_id = ids[0]

        # ── Run B ─────────────────────────────────────────────────────────────
        tui = _NativeTUI(binary=binary, build=False, workspace=workspace, fresh=False,
                         allowed_tools=ALLOWED, env_overrides=env,
                         extra_args=["--session", session_id])
        tui.screens = screens
        stack.callback(tui.stop)
        tui.start()
        tui.child.delaybeforesend = 0
        screen = _wait(tui, lambda value: "Ready" in value and VISIBLE[:20] in value
                       and PROMPT_A in value, "Run B ready on the restored session")
        frame(tui, "03-run-b-restored", screen)
        submit(tui, PROMPT_B)
        await_post(4)
        # At 80 columns the restored session's title pushes "Ready" off the status
        # bar; the turn is idle once its reply is on screen and persisted.
        screen = _wait(tui, lambda value: REPLY_B in _plain(value) and "···" not in value
                       and REPLY_B in _stored(workspace, session_id),
                       "Run B reply on screen and in the session store")
        frame(tui, "04-run-b-reply", screen)
        status, signal = tui.quit()
        assert (status, signal) == (0, None), (status, signal)
        spawns[-1]["exit_status"] = status

        with lock:
            observed = list(requests)
            replies = dict(posts)
        (evidence / "requests.json").write_text(json.dumps(observed, indent=2) + "\n")
        for index, post in sorted(replies.items()):
            (evidence / f"gemini-request-{index}.json").write_text(
                json.dumps(post["body"], indent=2) + "\n")
        safety["spawns"] = spawns
        safety["session_id"] = session_id
        safety["mcp_fixture"] = str(MCP_FIXTURE)
        safety["exposed_tool_names"] = names
        write_safety()

        # ── Assertions ────────────────────────────────────────────────────────
        assert not duplicates, f"responses requested more than once: {duplicates}"
        assert len(names) == 2, f"MCP tools not declared on POST 1: {names}"
        assert sorted(replies) == [1, 2, 3, 4], sorted(replies)
        auxiliary = [row for row in observed
                     if urlsplit(row["path"]).path == STREAM and row.get("response_index") is None]
        assert all('"tools"' not in row["body"] for row in auxiliary), auxiliary
        for post in replies.values():
            assert post["authorization"] == f"Bearer {ACCESS}"
            assert post["raw"].startswith(
                f'{{"project":"{PROJECT}","model":"{MODEL}","request":{{"contents":['), post["raw"][:200]
        assert all(urlsplit(r["path"]).path in {TOKEN, USERINFO, LOAD, ONBOARD, OPERATION,
                                                "/v1internal:fetchAvailableModels",
                                                "/v1internal:retrieveUserQuota",
                                                "/api.json", STREAM} for r in observed), observed
        image, echo = names["image"], names["echo"]
        third = replies[3]["body"]["request"]["contents"]
        fourth = replies[4]["body"]["request"]["contents"]
        models = [(at, content) for at, content in enumerate(third) if content["role"] == "model"]
        assert len(models) == 2, third
        (first_at, first_model), (second_at, second_model) = models
        assert first_model["parts"] == [
            {"thought": True, "text": THOUGHT, "thoughtSignature": SIGNATURES[0]},
            {"text": VISIBLE, "thoughtSignature": SIGNATURES[1]},
            {"functionCall": {"name": image, "args": {}}, "thoughtSignature": SIGNATURES[2]},
            {"functionCall": {"name": echo, "args": {"message": ECHO_FIRST}}},
        ], first_model
        results = third[first_at + 1]
        assert results["role"] == "user" and second_at == first_at + 2, third
        assert [part["functionResponse"]["name"] for part in results["parts"]] == [image, echo], results
        image_response = results["parts"][0]["functionResponse"]
        assert image_response["parts"] == [
            {"inlineData": {"mimeType": "image/png", "data": PNG_1X1_TRANSPARENT_B64}}], image_response
        assert ECHO_FIRST in results["parts"][1]["functionResponse"]["response"]["output"], results
        assert second_model["parts"] == [
            {"functionCall": {"name": echo, "args": {"message": ECHO_SECOND}},
             "thoughtSignature": SIGNATURES[3]}], second_model
        assert len(third) == second_at + 2, third
        last_result = third[second_at + 1]
        assert [part["functionResponse"]["name"] for part in last_result["parts"]] == [echo], last_result
        assert ECHO_SECOND in last_result["parts"][0]["functionResponse"]["response"]["output"]
        raw_third = replies[3]["raw"]
        for signature in SIGNATURES:
            assert raw_third.count(signature) == 1, signature
        assert raw_third.count(PNG_1X1_TRANSPARENT_B64) == 1

        # The reload oracle: POST 4 replays POST 3's contents element for element.
        prefix = [json.dumps(content, separators=(",", ":")) for content in third]
        replayed = [json.dumps(content, separators=(",", ":")) for content in fourth[:len(third)]]
        oracle = {"post_3_contents": third, "post_4_contents": fourth,
                  "prefix_equal": prefix == replayed}
        (evidence / "reload-oracle.json").write_text(json.dumps(oracle, indent=2) + "\n")
        assert prefix == replayed, "POST 4 does not replay POST 3's contents (see reload-oracle.json)"
        assert len(fourth) == len(third) + 2, fourth
        assert fourth[len(third)] == {"role": "model", "parts": [{"text": FINAL}]}, fourth[len(third)]
        tail = fourth[-1]
        assert tail["role"] == "user" and any(PROMPT_B in part.get("text", "") for part in tail["parts"]), tail
        raw_fourth = replies[4]["raw"]
        assert raw_fourth.startswith(replies[3]["raw"].split('"request":{"contents":[', 1)[0])
        for signature in SIGNATURES:
            assert raw_fourth.count(signature) == 1, signature

        # Secrets stay on the wire and in the store, never on any surface.
        markers = [*SIGNATURES, PNG_1X1_TRANSPARENT_B64]
        terminal.flush()
        surfaces = {"native.terminal.txt": (evidence / "native.terminal.txt").read_text(),
                    "native.screens.jsonl": (evidence / "native.screens.jsonl").read_text(),
                    **{path.name: path.read_text(errors="replace")
                       for path in sorted(evidence.glob("native.runtime.log*"))}}
        assert any(name.startswith("native.runtime.log") for name in surfaces), sorted(surfaces)
        surfaces.update({f"frame {n}": value for n, value in enumerate(frames)})
        for surface, text in surfaces.items():
            for marker in markers:
                assert marker not in text, f"{marker} leaked into {surface}"
        store = workspace / ".claude/sessions"
        stored = _stored(workspace, session_id)
        png = b64decode(PNG_1X1_TRANSPARENT_B64)
        stored_png = PNG_1X1_TRANSPARENT_B64 in stored or any(
            path.read_bytes() == png for path in store.rglob("*") if path.is_file())
        shutil.copytree(store, evidence / "session-store")
        for signature in SIGNATURES:
            assert signature in stored, f"{signature} missing from the session store"
        assert stored_png, "image bytes missing from the session store"


@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(300)
def test_gemini_tool_turn_survives_tui_reload_80x24(monkeypatch):
    parent = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())).resolve()
    parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="20-1d4a-", dir=parent))
    manifest = {"run_id": evidence.name, "started_at": datetime.now(timezone.utc).isoformat(),
                "evidence_dir": str(evidence), "viewport": [80, 24], "result": "RUNNING"}
    try:
        binary = harness._resolve_binary()[0].resolve()
        assert binary.is_file() and "debug" in binary.parts and "release" not in binary.parts, binary
        manifest.update(binary=str(binary), binary_sha256=_sha256(binary))
        (evidence / "run.json").write_text(json.dumps(manifest, indent=2) + "\n")
        with tempfile.TemporaryDirectory(prefix="r20-1d4a-") as root:
            _exercise(binary, evidence, Path(root), monkeypatch)
        assert _sha256(binary) == manifest["binary_sha256"], "binary changed during native smoke"
        manifest["result"] = "PASS"
    except pytest.skip.Exception as skipped:
        manifest["result"] = "SKIP"
        manifest["skip_reason"] = str(skipped)
        raise
    except BaseException:
        manifest["result"] = "FAIL"
        (evidence / "failure.txt").write_text(traceback.format_exc())
        raise
    finally:
        manifest["completed_at"] = datetime.now(timezone.utc).isoformat()
        (evidence / "run.json").write_text(json.dumps(manifest, indent=2) + "\n")
        print(f"NATIVE_20_1D4A_EVIDENCE={evidence}", flush=True)
