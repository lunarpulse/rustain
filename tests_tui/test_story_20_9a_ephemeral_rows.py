"""Story 20-9a K5: real PTY cancellation, plan card, quit and restart."""
from contextlib import ExitStack
from datetime import datetime, timezone
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import queue
import re
import shutil
import tempfile
import threading
import time
import traceback
from urllib.parse import urlsplit

import harness
import pexpect
import pytest

KEY = "sk-ant-20-9a-loopback-key-canary"
PARTIAL = "K5-CANCELLED-PARTIAL-CANARY"
QUIT_PARTIAL = "K5-QUIT-PARTIAL-CANARY"
CONTROL = "K5 completed control reply"
CARD = "K5 transient plan title"
TASK = "K5 durable plan task"
PROMPT_A = "k5 start held response"
PROMPT_B = "k5 complete control response"
PROMPT_PLAN = "k5 propose the plan"
PROMPT_QUIT = "k5 hold for quit"
PROMPT_RELOAD = "k5 continue after restart"
RELOAD_REPLY = "K5 restart reply complete"


def _event(obj):
    return (f"event: {obj['type']}\ndata: {json.dumps(obj, separators=(',', ':'))}\n\n").encode()


def _start_text(text):
    return b"".join(map(_event, [
        {"type": "message_start", "message": {"usage": {"input_tokens": 5}}},
        {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}},
        {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}},
    ]))


def _finish(reason="end_turn"):
    return b"".join(map(_event, [
        {"type": "content_block_stop", "index": 0},
        {"type": "message_delta", "delta": {"stop_reason": reason}, "usage": {"output_tokens": 5}},
        {"type": "message_stop"},
    ]))


def _plan_stream():
    payload = json.dumps({"title": CARD, "tasks": [{"title": TASK, "description": "durable task details"}]})
    return b"".join(map(_event, [
        {"type": "message_start", "message": {"usage": {"input_tokens": 5}}},
        {"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "toolu_k5_plan", "name": "propose_plan"}},
        {"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": payload}},
    ])) + _finish("tool_use")


class _NativeTUI(harness.RustainTUI):
    def _sync_screen(self):
        self.check_server()
        try:
            data = self.child.read_nonblocking(size=65536, timeout=0.2)
        except pexpect.TIMEOUT:
            assert self.child.isalive(), "native TUI exited unexpectedly"
            return
        except pexpect.EOF:
            pytest.fail("native TUI exited before expected screen")
        self._stream.feed(data)
        self.screens.write(json.dumps({"screen": "\n".join(self._screen.display)}) + "\n")
        self.screens.flush()

    def wait_for_screen(self, text, timeout=60, poll_interval=0.5):
        _wait(self, lambda screen: text in screen, text, timeout)
        return True

    def quit(self):
        self.child.sendcontrol("q")
        while True:
            try:
                data = self.child.read_nonblocking(size=65536, timeout=30)
            except pexpect.EOF:
                break
            self._stream.feed(data)
        self.child.close()
        return self.child.exitstatus, self.child.signalstatus

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


def _capture(tui, evidence, name, screen):
    assert tui._screen.columns == 80 and len(tui._screen.display) == 24
    assert screen == "\n".join(tui._screen.display)
    (evidence / f"{name}.screen.txt").write_text(screen + "\n")


def _submit(tui, prompt):
    tui.send(prompt)
    _wait(tui, lambda screen: prompt in screen, "typed prompt")
    tui.send("\r")


def _message_texts(body):
    return json.dumps(body.get("messages", []), ensure_ascii=False)


def _exercise(binary, evidence, root, monkeypatch):
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    workspace, home = root / "workspace", root / "home"
    workspace.mkdir()
    home.mkdir()
    config, data = workspace / ".rustain", home / "data"
    config.mkdir()
    data.mkdir()
    # Prevent the shared harness from copying the developer's .env into the workspace.
    (workspace / ".env").write_text("")
    errors = queue.Queue()
    requests = []
    lock = threading.Lock()
    disconnected = {kind: threading.Event() for kind in ("cancel", "quit")}

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
                self.respond(b'{}' if path == "/api.json" else b'{"data":[],"has_more":false}')
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
                last = messages[-1] if messages else {}
                content = last.get("content", "")
                text = json.dumps(content, ensure_ascii=False)
                if not body.get("tools"):
                    kind = "auxiliary"
                elif PROMPT_RELOAD in text:
                    kind = "reload"
                elif "Plan rejected by user" in text:
                    kind = "rejection"
                elif PROMPT_QUIT in text:
                    kind = "quit"
                elif PROMPT_PLAN in text:
                    kind = "plan"
                elif PROMPT_B in text:
                    kind = "control"
                elif PROMPT_A in text:
                    kind = "cancel"
                else:
                    kind = "tool-result"
                with lock:
                    requests.append({"kind": kind, "body": body})
                held = kind in ("cancel", "quit")
                if kind == "plan":
                    stream = _plan_stream()
                else:
                    answer = {"cancel": PARTIAL, "control": CONTROL, "quit": QUIT_PARTIAL,
                              "reload": RELOAD_REPLY, "tool-result": "K5 plan tool completed",
                              "rejection": "K5 rejection acknowledged", "auxiliary": "K5 session"}[kind]
                    stream = _start_text(answer) + (b"" if held else _finish())
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Transfer-Encoding", "chunked")
                self.end_headers()
                self.wfile.write(f"{len(stream):x}\r\n".encode() + stream + b"\r\n")
                self.wfile.flush()
                if held:
                    self.connection.settimeout(120)
                    try:
                        tail = self.connection.recv(1)
                    except ConnectionResetError:
                        tail = b""  # Expected cancellation may use TCP reset.
                    assert tail == b"", "held provider stream was not cancelled"
                    disconnected[kind].set()
                    self.close_connection = True
                    return
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
            check_server()

        stack.callback(close_server)
        origin = f"http://127.0.0.1:{server.server_port}"
        loaded = harness._load_env()
        cleared = {name for name in loaded if re.search(
            r"(?:API_KEY|AUTH_TOKEN|ACCESS_TOKEN|BASE_URL|ENDPOINT|PROXY|CREDENTIALS|PROJECT|PROJECT_ID)$", name, re.I)
            or name.startswith("RUSTAIN_")
            or name in {"OLLAMA_HOST", "OPENAI_CODEX_OAUTH_TOKEN"}}
        env = {name: "" for name in cleared}
        env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "xdg-config"),
                   XDG_DATA_HOME=str(home / "xdg-data"), XDG_CACHE_HOME=str(home / "xdg-cache"),
                   RUSTAIN_CONFIG_DIR=str(config), RUSTAIN_DATA_DIR=str(data),
                   RUSTAIN_LOG_PATH=str(evidence / "native.runtime.log"), RUST_LOG="debug",
                   ANTHROPIC_API_KEY=KEY, ANTHROPIC_AUTH_TOKEN="", ANTHROPIC_BASE_URL=origin,
                   RUSTAIN_MODELS_DEV_URL=origin, NO_PROXY="*", no_proxy="*", TERM="xterm-256color")
        terminal = stack.enter_context((evidence / "native.terminal.txt").open("w"))
        screens = stack.enter_context((evidence / "native.screens.jsonl").open("w"))
        original_spawn = harness.pexpect.spawn

        def spawn(command, *args, **kwargs):
            actual = kwargs["env"]
            assert Path(command).resolve() == binary and kwargs["dimensions"] == (24, 80)
            assert Path(kwargs["cwd"]) == workspace and actual["HOME"] == str(home)
            assert actual["ANTHROPIC_BASE_URL"] == origin and actual["ANTHROPIC_API_KEY"] == KEY
            assert all(actual.get(name) == "" for name in cleared if env[name] == "")
            kwargs["logfile"] = terminal
            return original_spawn(command, *args, **kwargs)

        monkeypatch.setattr(harness.pexpect, "spawn", spawn)

        def start(extra=None):
            tui = _NativeTUI(binary=binary, build=False, workspace=workspace, fresh=extra is None,
                             allowed_tools=["propose_plan"], env_overrides=env, extra_args=extra)
            tui.screens, tui.check_server = screens, check_server
            stack.callback(tui.stop)
            tui.start()
            tui.child.delaybeforesend = 0
            return tui

        tui = start()
        _submit(tui, PROMPT_A)
        screen = _wait(tui, lambda s: PARTIAL in s, "held partial")
        _capture(tui, evidence, "01-held-partial", screen)
        tui.child.sendcontrol("c")
        screen = _wait(tui, lambda s: PARTIAL in s and "[interrupted]" in s and "Ready" in s,
                       "cancelled partial and idle")
        _capture(tui, evidence, "02-cancelled-idle", screen)
        closed = disconnected["cancel"].wait(30)
        check_server()
        assert closed, "cancel did not close its provider stream"
        _submit(tui, PROMPT_B)
        screen = _wait(tui, lambda s: CONTROL in s and "Ready" in s, "completed control")
        _capture(tui, evidence, "03-control", screen)
        _submit(tui, PROMPT_PLAN)
        screen = _wait(tui, lambda s: "Plan:" in s and CARD in s and TASK in s, "rendered plan card")
        _capture(tui, evidence, "04-plan-card", screen)
        _wait(tui, lambda s: "K5 plan tool completed" in s and "Ready" in s,
              "plan tool response completed")
        tui.send("n")
        _wait(tui, lambda s: "rejected" in s.lower(), "plan rejection")
        _wait(tui, lambda s: "K5 rejection acknowledged" in s and "Ready" in s,
              "plan rejection response completed")
        _submit(tui, PROMPT_QUIT)
        screen = _wait(tui, lambda s: QUIT_PARTIAL in s, "quit held partial")
        _capture(tui, evidence, "05-quit-partial", screen)
        assert tui.quit() == (0, None)
        closed = disconnected["quit"].wait(30)
        check_server()
        assert closed, "quit did not close its provider stream"
        check_server()

        store = workspace / ".claude" / "sessions"
        ids = sorted({p.name[:-len(".meta.json")] if p.name.endswith(".meta.json") else p.name
                      for p in store.iterdir() if p.is_dir() or p.name.endswith(".meta.json")})
        assert len(ids) == 1, ids
        session_id = ids[0]
        payload_path = store / session_id / "conversation.json"
        if not payload_path.exists():
            payload_path = store / f"{session_id}.meta.json"
        payload = json.loads(payload_path.read_text())
        serialized = json.dumps(payload, ensure_ascii=False)
        assert CONTROL in serialized and PARTIAL not in serialized and QUIT_PARTIAL not in serialized
        plans = payload["plans"]
        matching = [p for p in (plans.values() if isinstance(plans, dict) else plans) if p["title"] == CARD]
        assert len(matching) == 1 and matching[0]["status"].lower() == "rejected", matching
        assert matching[0]["tasks"][0]["title"] == TASK and matching[0]["tasks"][0]["status"].lower() == "pending"
        assert not matching[0].get("hostMessageId")
        assert "ephemeralDisplay" not in serialized
        shutil.copytree(store, evidence / "session-store")

        tui = start(["--session", session_id])
        tui.send("\x1b")
        _wait(tui, lambda _s: tui._screen.cursor.hidden, "chat focus")
        tui.send("g")
        screen = _wait(tui, lambda s: CONTROL in s, "restored durable control")
        _capture(tui, evidence, "06-restored", screen)
        assert PARTIAL not in screen and QUIT_PARTIAL not in screen
        # A durable tool invocation may still show its input title; it must not
        # reconstruct the transient PlanCard host or its action footer.
        assert "Plan:" not in screen and "[y] Approve" not in screen
        # The absence oracle deliberately read the restored top. Return to the
        # real Following viewport before expecting a newly appended reply.
        tui.send("G")
        _wait(tui, lambda s: PROMPT_QUIT in s, "restored transcript end")
        tui.send("i")
        _wait(tui, lambda _s: not tui._screen.cursor.hidden, "input focus")
        _submit(tui, PROMPT_RELOAD)
        screen = _wait(tui, lambda s: RELOAD_REPLY in s and "Ready" in s, "restart response")
        _capture(tui, evidence, "07-restart-response", screen)
        assert tui.quit() == (0, None)
        check_server()
        with lock:
            observed = list(requests)
        (evidence / "requests.json").write_text(json.dumps(observed, indent=2) + "\n")
        for kind in ("control", "reload"):
            selected = [r["body"] for r in observed if r["kind"] == kind]
            assert len(selected) == 1, observed
            text = _message_texts(selected[0])
            assert PARTIAL not in text and QUIT_PARTIAL not in text and "PlanCard" not in text
            # Tool input is durable; only a separate text row would leak the card.
            for message in selected[0].get("messages", []):
                if message.get("role") == "assistant":
                    content = message.get("content", [])
                    if isinstance(content, str):
                        assert CARD not in content
                    else:
                        assert all(CARD not in block.get("text", "") for block in content
                                   if isinstance(block, dict) and block.get("type") == "text")
            if kind == "reload":
                assert CONTROL in text and PROMPT_RELOAD in text
        terminal.flush()
        screens.flush()
        logs = sorted(evidence.glob("native.runtime.log.*"))
        assert any(path.is_file() and re.fullmatch(r"native\.runtime\.log\.\d{4}-\d{2}-\d{2}", path.name)
                   for path in logs), "dated debug log missing"
        for path in [evidence / "native.terminal.txt", evidence / "native.screens.jsonl", *logs]:
            assert KEY not in path.read_text(errors="replace"), f"credential leaked to {path}"


@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(300)
def test_ephemeral_rows_cancel_quit_and_restart_80x24(monkeypatch):
    binary = harness._resolve_binary()[0].resolve()
    assert binary.is_file() and "debug" in binary.parts and "release" not in binary.parts, binary
    parent = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())).resolve()
    parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="20-9a-", dir=parent))
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    manifest = {"binary": str(binary), "sha256": digest, "viewport": [80, 24],
                "started_at": datetime.now(timezone.utc).isoformat(), "result": "RUNNING"}
    try:
        with tempfile.TemporaryDirectory(prefix="r20-9a-") as root:
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
        print(f"NATIVE_20_9A_EVIDENCE={evidence}", flush=True)
