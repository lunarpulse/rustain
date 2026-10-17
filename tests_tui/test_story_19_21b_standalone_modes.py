"""K02: real standalone Root must not copy the plan card's automatic send path."""
from contextlib import ExitStack
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

import harness
from fixtures.scene_provider import SceneStub
from harness import RustainTUI

pytestmark = pytest.mark.a2a_binary


def _wait(predicate, diagnostic, timeout=30):
    deadline = time.monotonic() + timeout
    while True:
        value = predicate()
        if value:
            return value
        assert time.monotonic() < deadline, diagnostic()
        time.sleep(0.04)


def _pending_card(screen):
    rows = [row.split("║")[1] for row in screen.splitlines() if row.count("║") >= 2]
    return "Awaitingyourdecision." in "".join(char for row in rows for char in row if not char.isspace())


def _command(tui, text):
    tui.send("\x1b[200~" + text + "\x1b[201~")
    tui.send("\r")


def _stop(process):
    if process.poll() is None:
        os.killpg(process.pid, signal.SIGTERM)
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
    process.wait()


def _events(workspace, name):
    found = []
    def visit(value):
        if isinstance(value, dict):
            if value.get("event") == name:
                found.append(value)
            for child in value.values():
                visit(child)
        elif isinstance(value, list):
            for child in value:
                visit(child)
    for path in workspace.rglob("*.jsonl"):
        for line in path.read_text().splitlines():
            visit(json.loads(line))
    return found


class _ObservedHttp:
    def __init__(self, *, ssl_context=None):
        self.backend = ""
        self.lock = threading.Lock()
        self.counts = {"get": 0, "post": 0}
        owner = self
        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                self.forward()
            def do_POST(self):
                self.forward()
            def forward(self):
                body = self.rfile.read(int(self.headers.get("Content-Length", "0"))) if self.command == "POST" else None
                with owner.lock:
                    owner.counts[self.command.lower()] += 1
                request = urllib.request.Request(
                    owner.backend + self.path, data=body, method=self.command,
                    headers={key: value for key, value in self.headers.items()
                             if key.lower() not in {"host", "connection", "content-length"}},
                )
                try:
                    response = urllib.request.urlopen(request, timeout=20, context=ssl_context)
                except urllib.error.HTTPError as error:
                    response = error
                with response:
                    payload = response.read()
                    self.send_response(response.status)
                    for key, value in response.headers.items():
                        if key.lower() not in {"connection", "transfer-encoding", "content-length"}:
                            self.send_header(key, value)
                    self.send_header("Content-Length", str(len(payload)))
                    self.end_headers()
                    self.wfile.write(payload)
            def log_message(self, *_):
                pass
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.port = self.server.server_address[1]
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def reset(self):
        with self.lock:
            self.counts = {"get": 0, "post": 0}

    def snapshot(self):
        with self.lock:
            return dict(self.counts)

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()


@pytest.mark.parametrize("mode", ["yolo", "autoedit"])
def test_standalone_auto_mode_still_requires_human_send(tmp_path, mode):
    binary = Path(os.environ.get("RUSTAIN_TUI_BINARY", harness.BINARY)).resolve()
    sender = tmp_path / "sender"
    sender.mkdir()
    sender_home = tmp_path / "sender-home"
    sender_home.mkdir()
    control = sender / "mode-control.txt"
    body = "ADR-047: Root must wait for a human Send key."
    def response(expect, content, stop_reason):
        return {"expect": expect, "content": content, "stop_reason": stop_reason,
                "usage": {"input_tokens": 20, "output_tokens": 20}}
    scene = tmp_path / "scene.json"
    scene.write_text(json.dumps({"scene": "19-21b-k02-standalone-modes", "personas": {
        "sender": {"turns": [
            response("write-control", [{"type": "tool_use", "name": "Write", "input": {
                "file_path": str(control), "content": mode + " Write completed without approval"}}], "tool_use"),
            response("mode-control.txt", [{"type": "text", "text": "WRITE_CONTROL_DONE"}], "end_turn"),
            response("raise-proposal", [{"type": "tool_use", "name": "propose_team_message", "input": {
                "recipients": ["lena-po"], "type": "architecture_update", "body": body}}], "tool_use"),
            response("Proposal shown to the operator.", [{"type": "text", "text": "PROPOSAL_TURN_DONE"}], "end_turn"),
            response("raise-clear-proposal", [{"type": "tool_use", "name": "propose_team_message", "input": {
                "recipients": ["lena-po"], "type": "architecture_update",
                "body": body + " Clear must withdraw this second draft."}}], "tool_use"),
            response("Proposal shown to the operator.", [{"type": "text", "text": "CLEAR_PROPOSAL_TURN_DONE"}], "end_turn"),
            response("after-clear-control", [{"type": "text", "text": "AFTER_CLEAR_CONTROL_DONE"}], "end_turn"),
        ]}, "recipient": {"turns": []},
    }}))
    with ExitStack() as resources:
        stub = resources.enter_context(SceneStub(scene, tmp_path / "requests.jsonl"))
        runtime_root = Path(resources.enter_context(tempfile.TemporaryDirectory(prefix="tp-k02-")))
        observer = _ObservedHttp()
        resources.callback(observer.close)
        recipient = tmp_path / "recipient"
        recipient.mkdir()
        recipient_home = tmp_path / "recipient-home"
        recipient_home.mkdir()
        config = recipient / ".rustain"
        config.mkdir()
        (recipient / ".env").write_text("")
        (config / "a2a.json").write_text(json.dumps({"server": {
            "admission": "allow", "advertised_host": f"127.0.0.1:{observer.port}"}, "agents": {}}))
        (config / "a2a-interaction.toml").write_text('[interaction.defaults]\nresponse_mode = "notify-and-wait"\nnotification = "immediate"\n')
        (config / "config.toml").write_text("[permissions]\nalways_tools = []\n")
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            port = reservation.getsockname()[1]
        observer.backend = f"http://127.0.0.1:{port}"
        log_path = tmp_path / "recipient.log"
        log = resources.enter_context(log_path.open("w"))
        env = {**os.environ, **stub.env("recipient", recipient_home),
               "RUSTAIN_CONFIG_DIR": str(config), "RUSTAIN_DATA_DIR": str(runtime_root / "recipient")}
        process = subprocess.Popen([str(binary), f"--serve-a2a=127.0.0.1:{port}", "daemon", "start", "--foreground"],
                                   cwd=recipient, env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        resources.callback(_stop, process)
        def host_ready():
            assert process.poll() is None, log_path.read_text()
            try:
                with urllib.request.urlopen(observer.backend + "/.well-known/agent-card.json", timeout=1) as reply:
                    return reply.status == 200
            except (OSError, urllib.error.URLError):
                return False
        _wait(host_ready, lambda: log_path.read_text())
        (sender / ".env").write_text("")
        (sender / ".rustain").mkdir()
        (sender / ".rustain/a2a.json").write_text(json.dumps({"agents": {"lena-po": {"url": f"http://127.0.0.1:{observer.port}/"}}}))
        tui = resources.enter_context(RustainTUI(build=False, binary=binary, workspace=sender,
                                                allowed_tools=[], env_overrides={**stub.env("sender", sender_home),
                                                "RUSTAIN_DATA_DIR": str(runtime_root / "sender")}, timeout=30))
        raw = resources.enter_context((tmp_path / "terminal.txt").open("w"))
        tui.child.logfile_read = raw
        assert tui.wait_for_screen("Type a message to start.", 30), tui.get_screen_text()
        _wait(lambda: observer.snapshot()["get"] >= 1, tui.get_screen_text)
        _wait(lambda: any("peer_id=lena-po" in path.read_text(errors="replace") for path in sender.glob("rustain.log*")), tui.get_screen_text)
        observer.reset()
        tui.set_permission_mode(mode)
        _command(tui, "write-control")
        _wait(lambda: control.exists(), tui.get_screen_text)
        assert control.read_text() == mode + " Write completed without approval", "K02 ordinary Write control failed"
        assert tui.wait_for_screen("WRITE_CONTROL_DONE", 30), tui.get_screen_text()
        _command(tui, "raise-proposal")
        _wait(lambda: _pending_card(tui.get_screen_text()) or _events(sender, "team_send_action_recorded"), tui.get_screen_text)
        assert not _events(sender, "team_send_action_recorded"), "K02 automatic mode sent the proposal without a human key"
        assert not _events(sender, "remote_envelope_dispatched"), "K02 automatic mode dispatched the proposal without a human key"
        assert observer.snapshot() == {"get": 0, "post": 0}, "K02 automatic mode contacted the recipient before human approval"
        assert _pending_card(tui.get_screen_text()), "K02 proposal card missing before a human decision"
        (tmp_path / "pending-card.txt").write_text(tui.get_screen_text())
        tui.send("y")
        _wait(lambda: observer.snapshot()["post"] == 1 and len(_events(sender, "remote_envelope_dispatched")) == 1, tui.get_screen_text)
        assert observer.snapshot() == {"get": 0, "post": 1}, "K02 human Send must make exactly one recipient POST"
        assert len(_events(sender, "team_send_action_recorded")) == 1
        assert tui.wait_for_screen("PROPOSAL_TURN_DONE", 30), tui.get_screen_text()
        _command(tui, "raise-clear-proposal")
        _wait(lambda: _pending_card(tui.get_screen_text()), tui.get_screen_text)
        assert tui.wait_for_screen("CLEAR_PROPOSAL_TURN_DONE", 30), tui.get_screen_text()
        _command(tui, "/clear")
        # A completed turn in the replacement conversation drains the real Root
        # event queue; a stale or retargeted card cannot hide behind an old frame.
        _command(tui, "after-clear-control")
        assert tui.wait_for_screen("AFTER_CLEAR_CONTROL_DONE", 30), tui.get_screen_text()
        assert not _pending_card(tui.get_screen_text()), "K03 clear must withdraw rather than retarget a proposal"
        assert observer.snapshot() == {"get": 0, "post": 1}, "K03 clear must not send or preflight the withdrawn proposal"
        assert len(_events(sender, "team_send_action_recorded")) == 1
        assert len(_events(sender, "remote_envelope_dispatched")) == 1
        assert not any(row.get("desync") or row.get("kind") == "desync" for row in stub.rows()), stub.rows()
