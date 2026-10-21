"""Story 19.20c K08: real binaries, terminal paint, and executor side effects.

The provider scripts inputs only. Permission records, cards, Review, decisions,
transport and execution all belong to the real product. Set RUSTAIN_TUI_BINARY
and optionally RUSTAIN_19_20C_EVIDENCE to retain native transcripts/screens.
"""
from contextlib import ExitStack, contextmanager
import json
import os
from pathlib import Path
import re
import shlex
import signal
import socket
import subprocess
import threading
import time
import urllib.error
import urllib.request

import pytest

import harness
from fixtures.scene_provider import SceneStub
from harness import RustainTUI

pytestmark = pytest.mark.a2a_binary
MARKER = "LATE-19-20C-EXACT-TAIL"
VIEWPORTS = [(60, 16), (80, 24), (130, 30)]


def _wait(predicate, diagnostic, timeout=30):
    deadline = time.monotonic() + timeout
    while True:
        value = predicate()
        if value:
            return value
        assert time.monotonic() < deadline, diagnostic()
        time.sleep(0.04)


def _compact(text):
    return "".join(text.split())


def _stop(process):
    if process.poll() is None:
        os.killpg(process.pid, signal.SIGTERM)
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
    process.wait()


class Evidence:
    def __init__(self, tag):
        root = os.environ.get("RUSTAIN_19_20C_EVIDENCE")
        self.root = Path(root) / tag / str(time.time_ns()) if root else None
        if self.root:
            self.root.mkdir(parents=True, exist_ok=True)

    def text(self, name, text):
        if self.root:
            (self.root / name).write_text(text)

    def terminal(self, stack, tui, name):
        if self.root:
            tui.child.logfile_read = stack.enter_context((self.root / name).open("w"))

    def screen(self, tui, name):
        text = tui.get_screen_text()
        self.text(name + ".screen.txt", text)
        return text


def _command(tui, text="run it"):
    tui.send("\x1b[200~" + text + "\x1b[201~")
    tui.send("\r")


def _bash(canary):
    head = "x" * 170
    command = "printf '%s\\n' " + shlex.quote(head) + " >> " + shlex.quote(str(canary))
    command += "\n" + "\n".join(": exact-line-%02d" % n for n in range(30))
    command += "\nprintf '%s\\n' " + MARKER + " >> " + shlex.quote(str(canary))
    return {"command": command}, (head + "\n" + MARKER + "\n").encode()


def _scene(path, tool, invocation):
    def turn(expect, content, stop):
        return {"expect": expect, "content": content, "stop_reason": stop,
                "usage": {"input_tokens": 10, "output_tokens": 10}}
    path.write_text(json.dumps({"scene": "19-20c-native", "personas": {"operator": {"turns": [
        turn("run it", [{"type": "tool_use", "name": tool, "input": invocation}], "tool_use"),
        turn("e", [{"type": "text", "text": "tool result received"}], "end_turn"),
    ]}}}))


@contextmanager
def _world(tmp_path, tool, invocation, evidence):
    with ExitStack() as stack:
        home, ws = tmp_path / "home", tmp_path / "ws"
        home.mkdir()
        (ws / ".rustain").mkdir(parents=True)
        (ws / ".env").write_text("")
        (ws / ".rustain/config.toml").write_text("[permissions]\nalways_tools = []\n")
        scene = tmp_path / "scene.json"
        _scene(scene, tool, invocation)
        stub = stack.enter_context(SceneStub(scene, tmp_path / "requests.jsonl"))
        env = {**os.environ, **stub.env("operator", home),
               "RUSTAIN_CONFIG_DIR": str(ws / ".rustain"),
               "RUSTAIN_DATA_DIR": str(tmp_path / "data"),
               "RUSTAIN_LOG_PATH": str(tmp_path / "runtime.log")}
        evidence.text("input.json", json.dumps(invocation, ensure_ascii=False, indent=2))
        try:
            yield stack, ws, stub, env
        finally:
            evidence.text("provider-requests.jsonl", (tmp_path / "requests.jsonl").read_text())
            assert not any(row.get("desync") or row.get("kind") == "desync" for row in stub.rows()), stub.rows()


def _tui(stack, ws, env, evidence, attached=False, name="writer"):
    tui = stack.enter_context(RustainTUI(build=False, fresh=not attached, workspace=ws,
                                       allowed_tools=[], env_overrides=env,
                                       extra_args=["daemon", "attach"] if attached else None))
    evidence.terminal(stack, tui, name + ".terminal.txt")
    return tui


def _card(tui, tool, evidence):
    _wait(lambda: "[r] Review" in tui.get_screen_text(), tui.get_screen_text, 60)
    text = evidence.screen(tui, "card")
    assert tool in text and "┃" in text, text
    assert "│ command:" in text if tool == "Bash" else "│ file_path:" in text, text
    assert _compact("… more not shown — [r] Review first") in _compact(text), text
    for key in ("[y] Allow", "[s] Session", "[a] Always", "[n] Deny", "[f] Deny + feedback", "[r] Review"):
        assert _compact(key) in _compact(text), text
    body = [row for row in text.splitlines() if "┃ │ " in row]
    assert 1 <= len(body) <= 4, text


def _review(tui, tool, evidence):
    tui.send("r")
    _wait(lambda: "Exact tool input (not run yet)" in tui.get_screen_text(), tui.get_screen_text)
    for page in range(100):
        text = evidence.screen(tui, "review-%02d" % page)
        assert _compact("Exact tool input (not run yet) · " + tool) in _compact(text), text
        rows = text.splitlines()
        source = next(i for i, row in enumerate(rows) if "risk:" in row)
        footer = next(i for i, row in enumerate(rows) if "[y] Allow" in row)
        body = [row for row in rows[source + 1:footer] if row.strip()]
        assert body and all(row.lstrip().startswith("│ ") for row in body), text
        if MARKER in text:
            return text
        before = text
        tui.send("\x1b[6~")
        _wait(lambda: tui.get_screen_text() != before, tui.get_screen_text, 5)
    pytest.fail("Review did not reach exact input tail")


def _finished(stub):
    return any(row.get("turn") == 2 and row.get("tool_results") == 1 for row in stub.rows())


@pytest.mark.parametrize("viewport", VIEWPORTS, ids=["60x16", "80x24", "130x30"])
def test_standalone_bash_exact_review_approve_and_deny(tmp_path, monkeypatch, viewport):
    monkeypatch.setattr(harness, "TERM_COLS", viewport[0])
    monkeypatch.setattr(harness, "TERM_ROWS", viewport[1])
    for decision in ("y", "n"):
        leg = tmp_path / decision
        leg.mkdir()
        canary = leg / "canary.txt"
        invocation, expected = _bash(canary)
        evidence = Evidence("standalone-bash-%dx%d-%s" % (*viewport, decision))
        with _world(leg, "Bash", invocation, evidence) as (stack, ws, stub, env):
            tui = _tui(stack, ws, env, evidence)
            _command(tui)
            _card(tui, "Bash", evidence)
            assert not canary.exists()
            if decision == "y":
                _review(tui, "Bash", evidence)
                assert not canary.exists(), "Review must not execute"
            tui.send(decision)
            _wait(lambda: _finished(stub), tui.get_screen_text)
            if decision == "y":
                assert canary.read_bytes() == expected, "exact command must execute once"
                evidence.text("executor-output.txt", canary.read_text())
            else:
                assert not canary.exists(), "denial must execute nothing"
                evidence.text("outcome.json", json.dumps({"decision": "deny", "canary_exists": canary.exists()}))
            evidence.screen(tui, "decided")


@pytest.mark.parametrize("viewport", VIEWPORTS, ids=["60x16", "80x24", "130x30"])
@pytest.mark.parametrize("attached", [False, True], ids=["standalone", "rich-attached"])
def test_write_exact_review(tmp_path, monkeypatch, viewport, attached):
    monkeypatch.setattr(harness, "TERM_COLS", viewport[0])
    monkeypatch.setattr(harness, "TERM_ROWS", viewport[1])
    target = tmp_path / "ws" / "written.txt"
    content = "\n".join("exact write %03d · λ" % n for n in range(90)) + "\n" + MARKER
    invocation = {"file_path": str(target), "content": content}
    evidence = Evidence("%s-write-%dx%d" % ("rich-attached" if attached else "standalone", *viewport))
    with _world(tmp_path, "Write", invocation, evidence) as (stack, ws, stub, env):
        if attached:
            _daemon(stack, ws, env, tmp_path)
        tui = _tui(stack, ws, env, evidence, attached=attached)
        mirror = _observer(stack, ws, env, evidence) if attached else None
        _command(tui)
        _card(tui, "Write", evidence)
        if mirror:
            _observer_pending(mirror, evidence)
        _review(tui, "Write", evidence)
        assert not target.exists()
        tui.send("y")
        _wait(lambda: _finished(stub), tui.get_screen_text)
        assert target.read_bytes() == content.encode()
        evidence.text("executor-output.txt", target.read_text())
        evidence.screen(tui, "decided")


def _daemon(stack, ws, env, tmp_path):
    log = stack.enter_context((tmp_path / "daemon.log").open("w"))
    proc = subprocess.Popen([os.environ["RUSTAIN_TUI_BINARY"], "daemon", "start", "--foreground"],
                            cwd=ws, env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
    stack.callback(_stop, proc)
    def ready():
        assert proc.poll() is None, (tmp_path / "daemon.log").read_text()
        probe = subprocess.run([os.environ["RUSTAIN_TUI_BINARY"], "daemon", "status"], cwd=ws,
                               env=env, capture_output=True, text=True, timeout=5)
        return probe.returncode == 0 and "running" in (probe.stdout + probe.stderr).lower()
    _wait(ready, lambda: (tmp_path / "daemon.log").read_text())
    return proc


class Plain:
    def __init__(self, stack, ws, env, args=None):
        self.lines = []
        self.proc = subprocess.Popen([os.environ["RUSTAIN_TUI_BINARY"], *(args or ["daemon", "attach", "--plain"])],
                                     cwd=ws, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.STDOUT, text=True, bufsize=1, start_new_session=True)
        stack.callback(_stop, self.proc)
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.proc.stdout:
            self.lines.append(line.rstrip("\n"))

    def text(self):
        return "\n".join(self.lines)

    def send(self, line):
        self.proc.stdin.write(line + "\n")
        self.proc.stdin.flush()

    def wait(self, needle):
        return _wait(lambda: needle in self.text(), self.text, 60)


def _observer(stack, ws, env, evidence):
    mirror = _tui(stack, ws, env, evidence, attached=True, name="mirror")
    _wait(lambda: "Read-only" in mirror.get_screen_text(), mirror.get_screen_text)
    return mirror


def _observer_pending(mirror, evidence):
    # Pending tool input must not grant a read-only mirror writer controls.
    text = evidence.screen(mirror, "mirror-pending")
    assert "[y] Allow" not in text and "[r] Review" not in text, text
    assert "input withheld" not in text, text


@pytest.mark.parametrize("viewport", VIEWPORTS, ids=["60x16", "80x24", "130x30"])
def test_rich_attach_exact_review_and_trusted_mirror(tmp_path, monkeypatch, viewport):
    monkeypatch.setattr(harness, "TERM_COLS", viewport[0])
    monkeypatch.setattr(harness, "TERM_ROWS", viewport[1])
    canary = tmp_path / "canary.txt"
    invocation, expected = _bash(canary)
    evidence = Evidence("rich-attach-%dx%d" % viewport)
    with _world(tmp_path, "Bash", invocation, evidence) as (stack, ws, stub, env):
        _daemon(stack, ws, env, tmp_path)
        writer = _tui(stack, ws, env, evidence, attached=True)
        mirror = _observer(stack, ws, env, evidence)
        _command(writer)
        _card(writer, "Bash", evidence)
        _observer_pending(mirror, evidence)
        _review(writer, "Bash", evidence)
        assert not canary.exists()
        writer.send("y")
        _wait(lambda: _finished(stub), writer.get_screen_text)
        assert canary.read_bytes() == expected
        evidence.text("executor-output.txt", canary.read_text())
        _wait(lambda: "printf" in mirror.get_screen_text(), mirror.get_screen_text)
        text = evidence.screen(mirror, "mirror-tool-input")
        assert "input withheld" not in text and "[r] Review" not in text, text
        evidence.screen(writer, "decided")


def test_plain_attach_exact_review_retained_reissue(tmp_path):
    canary = tmp_path / "canary.txt"
    invocation, expected = _bash(canary)
    evidence = Evidence("plain-attach")
    with _world(tmp_path, "Bash", invocation, evidence) as (stack, ws, stub, env):
        _daemon(stack, ws, env, tmp_path)
        writer = Plain(stack, ws, env)
        writer.wait("attached (writer)")
        mirror = Plain(stack, ws, env)
        mirror.wait("attached (read-only")
        writer.send("run it")
        writer.wait("Type: approve ")
        request = next(line for line in writer.lines if line.startswith("[APPROVAL REQUESTED]"))
        match = re.fullmatch(r"\[APPROVAL REQUESTED\] tool=Bash request_id=(\S+)", request)
        assert match, writer.text()
        rid = match[1]
        index = writer.lines.index(request)
        assert writer.lines[index + 1] == "  " + invocation["command"].splitlines()[0] + "…", writer.text()
        assert writer.lines[index + 2] == "Type: approve <id> | deny <id> | review <id>", writer.text()
        writer.send("review " + rid)
        writer.wait("--- end " + rid + " ---")
        start = writer.lines.index(f"--- exact input for {rid} (not run yet) ---")
        end = writer.lines.index("--- end " + rid + " ---")
        body = writer.lines[start + 1:end]
        assert body and all(row.startswith("│ ") for row in body), writer.text()
        assert any(MARKER in row for row in body), writer.text()
        assert not canary.exists()
        evidence.text("writer-before-detach.txt", writer.text())
        writer.proc.stdin.close()
        writer.proc.wait(timeout=10)
        # CLI has no --read-only: the first new attach after detach is the new writer.
        replacement = Plain(stack, ws, env)
        replacement.wait("attached (writer)")
        replacement.wait("Type: approve <id> | deny <id> | review <id>")
        replacement.send("review " + rid)
        replacement.wait("--- end " + rid + " ---")
        assert MARKER in replacement.text()
        assert not canary.exists()
        assert "[APPROVAL REQUESTED]" not in mirror.text()
        replacement.send("approve " + rid)
        _wait(lambda: _finished(stub), replacement.text)
        assert canary.read_bytes() == expected
        evidence.text("executor-output.txt", canary.read_text())
        evidence.text("replacement-writer.txt", replacement.text())
        evidence.text("read-only-mirror.txt", mirror.text())


def test_acp_stdio_exact_permission_executes_once(tmp_path):
    canary = tmp_path / "canary.txt"
    invocation, expected = _bash(canary)
    evidence = Evidence("acp-stdio")
    with _world(tmp_path, "Bash", invocation, evidence) as (stack, ws, stub, env):
        acp = Plain(stack, ws, env, args=["acp"])
        def send(value):
            acp.send(json.dumps({"jsonrpc": "2.0", **value}))
        def frames():
            return [json.loads(line) for line in acp.lines if line.startswith("{")]
        def response(rid):
            return next((f for f in frames() if f.get("id") == rid and "result" in f), None)
        send({"id": 1, "method": "initialize", "params": {"protocolVersion": 1, "clientCapabilities": {}}})
        _wait(lambda: response(1), acp.text)
        send({"id": 2, "method": "session/new", "params": {"cwd": str(ws), "mcpServers": []}})
        session = _wait(lambda: response(2), acp.text)["result"]["sessionId"]
        send({"id": 3, "method": "session/prompt", "params": {"sessionId": session,
              "prompt": [{"type": "text", "text": "run it"}]}})
        permission = _wait(lambda: next((f for f in frames() if f.get("method") == "session/request_permission"), None), acp.text)
        call = permission["params"]["toolCall"]
        assert call["title"] == "Bash: " + invocation["command"].splitlines()[0] + "…"
        assert call["rawInput"] == invocation
        assert MARKER in json.dumps(call["content"])
        assert not canary.exists()
        option = next(o["optionId"] for o in permission["params"]["options"] if o["kind"] == "allow_once")
        send({"id": permission["id"], "result": {"outcome": {"outcome": "selected", "optionId": option}}})
        _wait(lambda: response(3), acp.text)
        assert canary.read_bytes() == expected
        evidence.text("executor-output.txt", canary.read_text())
        evidence.text("stdio.jsonl", acp.text())


@pytest.mark.parametrize("viewport", VIEWPORTS, ids=["60x16", "80x24", "130x30"])
@pytest.mark.parametrize("attached", [False, True], ids=["standalone", "rich-attached"])
def test_delegation_exact_review(tmp_path, monkeypatch, viewport, attached):
    monkeypatch.setattr(harness, "TERM_COLS", viewport[0])
    monkeypatch.setattr(harness, "TERM_ROWS", viewport[1])
    tool = "a2a__reviewer__native-review"
    invocation = {"message": "\n".join("delegation line %03d" % n for n in range(60)) + "\n" + MARKER}
    evidence = Evidence("%s-delegation-%dx%d" % ("rich-attached" if attached else "standalone", *viewport))
    with _world(tmp_path, tool, invocation, evidence) as (stack, ws, stub, env):
        peer = tmp_path / "peer"
        (peer / ".rustain").mkdir(parents=True)
        (peer / ".env").write_text("")
        (peer / ".rustain/a2a.json").write_text(json.dumps({"server": {"admission": "allow"}, "agents": {}}))
        skill = peer / ".agents/skills/native-review/SKILL.md"
        skill.parent.mkdir(parents=True)
        skill.write_text("---\nname: native-review\ndescription: Native approval review fixture\n---\nReview the supplied message.\n")
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            port = reservation.getsockname()[1]
        endpoint = f"http://127.0.0.1:{port}"
        log_path = tmp_path / "peer.log"
        log = stack.enter_context(log_path.open("w"))
        peer_env = {**env, "RUSTAIN_CONFIG_DIR": str(peer / ".rustain"),
                    "RUSTAIN_DATA_DIR": str(tmp_path / "peer-data"),
                    "ANTHROPIC_API_KEY": "peer-must-not-execute"}
        proc = subprocess.Popen([os.environ["RUSTAIN_TUI_BINARY"], f"--serve-a2a=127.0.0.1:{port}",
                                 "daemon", "start", "--foreground"], cwd=peer, env=peer_env,
                                stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        stack.callback(_stop, proc)
        def ready():
            assert proc.poll() is None, log_path.read_text()
            try:
                with urllib.request.urlopen(endpoint + "/.well-known/agent-card.json", timeout=1) as reply:
                    card = json.load(reply)
                    return card if any(s["id"] == "native-review" for s in card["skills"]) else None
            except (OSError, urllib.error.URLError):
                return None
        card = _wait(ready, lambda: log_path.read_text())
        evidence.text("peer-agent-card.json", json.dumps(card, indent=2))
        (ws / ".rustain/a2a.json").write_text(json.dumps({"agents": {"reviewer": {"url": endpoint + "/"}}}))
        if attached:
            _daemon(stack, ws, env, tmp_path)
        tui = _tui(stack, ws, env, evidence, attached=attached)
        mirror = _observer(stack, ws, env, evidence) if attached else None
        _wait(lambda: any("peer_id=reviewer" in path.read_text() for path in tmp_path.glob("runtime.log*")), tui.get_screen_text)
        _command(tui)
        _wait(lambda: "[r] Review" in tui.get_screen_text(), tui.get_screen_text)
        text = evidence.screen(tui, "card")
        assert "│ message:" in text, text
        if mirror:
            _observer_pending(mirror, evidence)
        _review(tui, tool, evidence)
        assert not _finished(stub), "Review must not resolve the delegation"
        tui.send("n")
        _wait(lambda: _finished(stub), tui.get_screen_text)
        evidence.screen(tui, "denied")
        turns = [row for row in stub.rows() if "turn" in row]
        assert [(row["persona"], row["turn"], row["tool_results"]) for row in turns] == [
            ("operator", 1, 0), ("operator", 2, 1),
        ], "denied delegation must never invoke the peer provider"
