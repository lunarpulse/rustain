"""19.18b: one debug-native 80x24 smoke, open question and cancelled inbound card."""
from contextlib import ExitStack
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
import traceback
import urllib.error
import urllib.request

import pytest
import harness
from fixtures.scene_provider import SceneStub
from test_story_19_20b_rap_one_way import _StructuralTUI, _sha256, _stop
from test_story_19_21_act1_distribution import _command

NOTICE = "[cancelled] This request was cancelled before you answered; no response was sent."


def _screen(tui, predicate, label, timeout=45):
    deadline = time.monotonic() + timeout
    while True:
        screen = tui.get_screen_text()  # waits on actual PTY output
        if predicate(screen):
            return screen
        assert time.monotonic() < deadline, f"{label}:\n{screen}"


def _rpc(endpoint, method, params):
    body = json.dumps({"jsonrpc": "2.0", "id": 7, "method": method, "params": params}).encode()
    request = urllib.request.Request(endpoint, body, {"Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=35) as response:
        return json.load(response)


def _message(message_id, task=None):
    message = {"kind": "message", "messageId": message_id, "role": "user",
               "parts": [{"kind": "text", "text": "native parked question"}]}
    if task is not None:
        message["taskId"] = task
    return {"message": message}


def _world(root, alias, stub, evidence):
    workspace = root / alias
    config = workspace / ".rustain"
    config.mkdir(parents=True)
    (workspace / ".env").write_text("")
    (config / "relay.json").write_text('{"mode":"disabled"}\n')
    home = root / (alias + "-home")
    home.mkdir()
    env = {**os.environ, **stub.env(alias, home), "TERM": "xterm-256color",
           "RUSTAIN_CONFIG_DIR": str(config), "RUSTAIN_DATA_DIR": str(workspace / ".rustain_data"),
           "RUSTAIN_LOG_PATH": str(evidence / (alias + ".runtime.log"))}
    return workspace, config, env


def _tui(stack, binary, workspace, env, evidence, name, attached=False):
    tui = _StructuralTUI(binary=binary, build=False, fresh=False, workspace=workspace,
                         env_overrides=env, extra_args=["daemon", "attach"] if attached else [])
    tui.screens = stack.enter_context((evidence / (name + ".screens.jsonl")).open("w"))
    terminal = stack.enter_context((evidence / (name + ".terminal.txt")).open("w"))
    stack.callback(tui.stop)
    tui.start()
    tui.child.delaybeforesend = 0
    tui.child.logfile_read = terminal
    return tui


def _outbound(stack, binary, root, stub, evidence):
    requests = []
    request_lock = threading.Lock()

    class Clarifier(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def reply(self, value):
            encoded = json.dumps(value).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(encoded)))
            self.end_headers()
            self.wfile.write(encoded)

        def do_GET(self):
            self.reply({"name": "Native Clarifier", "url": endpoint,
                        "protocolVersion": "0.3.0", "preferredTransport": "JSONRPC",
                        "version": "1", "capabilities": {"streaming": False},
                        "skills": [{"id": "clarify", "name": "Clarify", "description": "Ask one question", "tags": []}]})

        def do_POST(self):
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            with request_lock:
                requests.append(request)
            method = request["method"]
            state = {"message/send": "working", "tasks/get": "input-required", "tasks/cancel": "canceled"}[method]
            result = {"kind": "task", "id": "native-clarifier-task", "contextId": "native-context", "status": {"state": state}}
            if method == "tasks/get":
                result["status"]["message"] = {"kind": "message", "role": "agent", "parts": [{"kind": "text", "text": "main or replica?"}]}
            self.reply({"jsonrpc": "2.0", "id": request["id"], "result": result})

    peer = ThreadingHTTPServer(("127.0.0.1", 0), Clarifier)
    endpoint = f"http://127.0.0.1:{peer.server_address[1]}/"
    thread = threading.Thread(target=peer.serve_forever, daemon=True)
    thread.start()
    stack.callback(peer.server_close)
    stack.callback(peer.shutdown)
    workspace, config, env = _world(root, "operator", stub, evidence)
    (config / "a2a.json").write_text(json.dumps({"agents": {"clarifier": {"url": endpoint}}}) + "\n")
    (config / "config.toml").write_text('[permissions]\nalways_tools = []\n')
    tui = _tui(stack, binary, workspace, env, evidence, "outbound")
    deadline = time.monotonic() + 45
    while True:
        tui.get_screen_text()
        if any("peer_id=clarifier" in path.read_text() for path in evidence.glob("operator.runtime.log*")):
            break
        assert time.monotonic() < deadline, "clarifier discovery did not settle"
    _command(tui, "ask clarifier")
    approved = False
    deadline = time.monotonic() + 60
    while True:
        screen = tui.get_screen_text()
        if "Question remains open." in screen:
            break
        if not approved and "[r] Review" in screen:
            tui.send("y")
            approved = True
        assert time.monotonic() < deadline, f"outbound question:\n{screen}"
    # The Agents sidebar (including Dashboard's Agents selection) legitimately
    # requires >=120 columns. At 80 columns the real tool result is the closest
    # operator-visible proof: expand it using the normal transcript controls.
    tui.send("\x1b")
    _screen(tui, lambda _value: tui._screen.cursor.hidden, "transcript focus after Escape")
    tui.send("g")
    _screen(tui, lambda value: "a2a__clarifier__clarify" in value, "tool block in transcript")
    tui.send("]")
    tui.send("]")
    tui.get_screen_text()
    tui.send("\r")
    _screen(tui, lambda value: '"state": "input-required"' in value,
            "expanded tool result")
    # Expansion preserves the tail anchor; the handle is at the JSON's top.
    tui.send("g")
    tui.send("g")
    screen = _screen(tui, lambda value: '"state": "input-required"' in value
                     and '"continue": "a2a-continue-' in value,
                     "open question and single-use continuation in tool result")
    (evidence / "01-outbound-waiting.screen.txt").write_text(screen)
    with request_lock:
        observed = list(requests)
    assert [request["method"] for request in observed] == ["message/send", "tasks/get"], observed
    assert not any(request["method"] == "tasks/cancel" for request in observed), observed
    (evidence / "outbound-peer-requests.json").write_text(json.dumps(observed, indent=2) + "\n")


def _inbound(stack, binary, root, stub, evidence):
    workspace, config, env = _world(root, "recipient", stub, evidence)
    (config / "a2a.json").write_text('{"server":{"admission":"allow"},"agents":{}}\n')
    (config / "a2a-interaction.toml").write_text(
        '[interaction.defaults]\nresponse_mode = "notify-and-wait"\nnotification = "immediate"\n')
    reservation = socket.socket()
    reservation.bind(("127.0.0.1", 0))
    port = reservation.getsockname()[1]
    reservation.close()
    endpoint = f"http://127.0.0.1:{port}/"
    logfile = evidence / "recipient.daemon.log"
    log = stack.enter_context(logfile.open("w"))
    process = subprocess.Popen([str(binary), f"--serve-a2a=127.0.0.1:{port}", "daemon", "start", "--foreground"],
                               cwd=workspace, env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
    stack.callback(_stop, process)
    deadline = time.monotonic() + 45
    while True:
        assert process.poll() is None, logfile.read_text()
        status = subprocess.run([str(binary), "daemon", "status"], cwd=workspace, env=env,
                                capture_output=True, text=True, timeout=5)
        try:
            with urllib.request.urlopen(endpoint + ".well-known/agent-card.json", timeout=1) as response:
                served = response.status == 200
        except (OSError, urllib.error.URLError):
            served = False
        if served and status.returncode == 0 and "running" in (status.stdout + status.stderr).lower():
            break
        assert time.monotonic() < deadline, logfile.read_text() + status.stdout + status.stderr
    tui = _tui(stack, binary, workspace, env, evidence, "inbound", attached=True)
    task = "native-parked-inbound"
    initial = _rpc(endpoint, "message/send", _message(task))
    (evidence / "inbound-send.json").write_text(json.dumps(initial, indent=2) + "\n")
    approved = False
    deadline = time.monotonic() + 45
    while True:
        screen = tui.get_screen_text()
        if "[awaiting your response]" in screen:
            break
        if not approved and "[a] Always allow" in screen:
            tui.send("a")
            approved = True
        assert time.monotonic() < deadline, f"inbound pending card:\n{screen}"
    (evidence / "02-inbound-pending.screen.txt").write_text(screen)
    cancelled = _rpc(endpoint, "tasks/cancel", {"id": task})
    assert cancelled["result"]["status"]["state"] == "canceled", cancelled
    (evidence / "inbound-cancel.json").write_text(json.dumps(cancelled, indent=2) + "\n")

    def settled(screen):
        text = " ".join(screen.replace("│", " ").replace("┆", " ").split())
        return (NOTICE in text and "[awaiting your response]" not in screen
                and "Peer is waiting for your response" not in screen)

    screen = _screen(tui, settled, "cancelled transcript notice and no pending card")
    (evidence / "03-inbound-cancelled.screen.txt").write_text(screen)
    terminal = _rpc(endpoint, "message/send", _message("native-terminal-followup", task))
    assert terminal["error"] == {"code": -32004, "message": "Task is in a terminal state and cannot accept further messages"}, terminal
    (evidence / "inbound-terminal-followup.json").write_text(json.dumps(terminal, indent=2) + "\n")


def _exercise(binary, evidence, root, monkeypatch):
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    scene = evidence / "scene.json"
    scene.write_text(json.dumps({"scene": "19-18b-native", "personas": {
        "operator": {"turns": [
            {"expect": "ask clarifier", "content": [{"type": "tool_use", "name": "a2a__clarifier__clarify", "input": {"message": "clarify database"}}], "stop_reason": "tool_use", "usage": {"input_tokens": 10, "output_tokens": 10}},
            {"expect": "The remote agent asked for input", "content": [{"type": "text", "text": "Question remains open."}], "stop_reason": "end_turn", "usage": {"input_tokens": 10, "output_tokens": 10}}
        ]}, "recipient": {"turns": []}
    }}) + "\n")
    with ExitStack() as stack:
        stub = stack.enter_context(SceneStub(scene, evidence / "provider-requests.jsonl"))
        _outbound(stack, binary, root, stub, evidence)
        _inbound(stack, binary, root, stub, evidence)
        assert not any(row.get("desync") for row in stub.rows()), stub.rows()
        (evidence / "assertions.txt").write_text(
            "PASS: debug binary, one 80x24 native smoke, two legs.\n"
            "PASS: expanded model tool result shows input-required and a continuation handle; zero tasks/cancel while parked.\n"
            "PASS: attached inbound pending card cancels to canceled; card disappears and exact notice persists.\n"
            "PASS: native cancelled taskId continuation refuses with -32004; no provider scene desync.\n")


@pytest.mark.a2a_binary
@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(240)
def test_input_required_waiting_and_parked_cancel_80x24(monkeypatch):
    evidence_root = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())).resolve()
    evidence_root.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="19-18b-", dir=evidence_root))
    manifest = {"run_id": evidence.name, "started_at": datetime.now(timezone.utc).isoformat(),
                "evidence_dir": str(evidence), "viewport": [80, 24], "result": "RUNNING"}
    identity = evidence / "run.json"
    try:
        binary = Path(os.environ["RUSTAIN_TUI_BINARY"]).resolve()
        assert binary.is_file() and "release" not in binary.parts, binary
        manifest.update(binary=str(binary), binary_sha256=_sha256(binary))
        identity.write_text(json.dumps(manifest, indent=2) + "\n")
        with tempfile.TemporaryDirectory(prefix="r19-18b-", dir="/tmp") as root:
            _exercise(binary, evidence, Path(root), monkeypatch)
        assert _sha256(binary) == manifest["binary_sha256"], "binary changed during smoke"
        manifest["result"] = "PASS"
    except BaseException:
        manifest["result"] = "FAIL"
        (evidence / "failure.txt").write_text(traceback.format_exc())
        raise
    finally:
        manifest["completed_at"] = datetime.now(timezone.utc).isoformat()
        identity.write_text(json.dumps(manifest, indent=2) + "\n")
        print(f"NATIVE_19_18B_EVIDENCE={evidence}", flush=True)
