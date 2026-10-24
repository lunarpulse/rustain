"""Story 19.20: real 80x24 sender approvals and recipient provenance/actions."""
from contextlib import ExitStack
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import urllib.error
import urllib.request

import pytest
import harness
from harness import RustainTUI
from fixtures.scene_provider import SceneStub
from test_story_19_19_act2_consultation import _command, _events, _stop, _wait
from test_story_19_20a_authorship_marks import _sha256


@pytest.mark.a2a_binary
@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(240)
def test_act5_recipient_marks_and_bug_report_actions(monkeypatch):
    binary = Path(os.environ["RUSTAIN_TUI_BINARY"]).resolve()
    assert binary.is_file(), binary
    assert "release" not in binary.parts, "Story 19.20 smoke requires a debug binary"
    assert "debug" in binary.parts, binary
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    gate = Path("/tmp/19-20-gate")
    gate.mkdir(parents=True, exist_ok=True)
    attempt = Path(tempfile.mkdtemp(prefix="native-", dir=gate))
    before_hash = _sha256(binary)
    with tempfile.TemporaryDirectory(prefix="r20-", dir="/tmp") as directory:
        _exercise(binary, Path(directory), attempt)
    assert _sha256(binary) == before_hash, "debug binary changed during smoke"
    (attempt / "result.json").write_text(json.dumps({
        "result": "PASS", "viewport": "80x24", "binary": str(binary),
        "binary_sha256": before_hash,
        "assertions": ["Operator: agent-composed only", "Policy: both marks",
                       "typed bug_report: Acknowledge and Discuss", "no recipient provider turn"],
    }, indent=2) + "\n")
    evidence_root = os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR")
    if evidence_root:
        evidence = Path(evidence_root)
        evidence.mkdir(parents=True, exist_ok=True)
        shutil.copytree(attempt, evidence / attempt.name)


def _exercise(binary, root, evidence):
    invocation = {"message": "Review the supplied Act 5 message."}
    tool = "a2a__recipient__native-review"
    turns = []
    for prompt, done in [("FIRST-DELEGATION", "FIRST-COMPLETE"),
                         ("SECOND-DELEGATION", "SECOND-COMPLETE")]:
        turns.extend([
            {"expect": prompt, "content": [{"type": "tool_use", "name": tool,
              "input": invocation}], "stop_reason": "tool_use",
             "usage": {"input_tokens": 10, "output_tokens": 10}},
            {"expect": "Received", "content": [{"type": "text", "text": done}],
             "stop_reason": "end_turn", "usage": {"input_tokens": 10, "output_tokens": 10}},
        ])
    scene = evidence / "scene.json"
    scene.write_text(json.dumps({"scene": "19-20-act5-native", "personas": {
        "sender": {"turns": turns}, "recipient": {"turns": []},
    }}))
    with ExitStack() as stack:
        stub = stack.enter_context(SceneStub(scene, evidence / "requests.jsonl"))
        worlds = {}
        reservation = socket.socket()
        reservation.bind(("127.0.0.1", 0))
        port = reservation.getsockname()[1]
        stack.callback(reservation.close)
        endpoint = f"http://127.0.0.1:{port}/"
        for alias in ("sender", "recipient"):
            workspace = root / alias
            (workspace / ".rustain").mkdir(parents=True)
            (workspace / ".env").write_text("")
            home = root / (alias + "-home")
            home.mkdir()
            env = {**os.environ, **stub.env(alias, home), "TERM": "xterm-256color",
                   "RUSTAIN_CONFIG_DIR": str(workspace / ".rustain"),
                   "RUSTAIN_DATA_DIR": str(workspace / ".rustain_data"),
                   "RUSTAIN_LOG_PATH": str(evidence / (alias + ".runtime.log"))}
            peers = {"recipient": {"url": endpoint}} if alias == "sender" else {}
            (workspace / ".rustain/a2a.json").write_text(json.dumps({
                "server": {"admission": "allow"}, "agents": peers,
            }))
            (workspace / ".rustain/a2a-interaction.toml").write_text(
                '[interaction.defaults]\nresponse_mode = "notify-and-auto"\nnotification = "immediate"\n')
            worlds[alias] = workspace, env
        workspace, env = worlds["recipient"]
        skill = workspace / ".agents/skills/native-review/SKILL.md"
        skill.parent.mkdir(parents=True)
        skill.write_text("---\nname: native-review\ndescription: Native delegation fixture\n---\nReview the supplied message.\n")
        reservation.close()
        daemon_log = stack.enter_context((evidence / "recipient.daemon.log").open("w"))
        daemon = subprocess.Popen([
            str(binary), f"--serve-a2a=127.0.0.1:{port}", "daemon", "start", "--foreground",
        ], cwd=workspace, env=env, stdout=daemon_log, stderr=subprocess.STDOUT, start_new_session=True)
        stack.callback(_stop, daemon)

        def advertised():
            assert daemon.poll() is None, "recipient daemon exited"
            try:
                with urllib.request.urlopen(endpoint + ".well-known/agent-card.json", timeout=1) as response:
                    card = json.load(response)
                    return card if any(s["id"] == "native-review" for s in card["skills"]) else None
            except (OSError, urllib.error.URLError):
                return None
        card = _wait(advertised, lambda: "native-review not advertised")
        (evidence / "recipient-agent-card.json").write_text(json.dumps(card, indent=2))

        def start_tui(alias, attached):
            ws, environment = worlds[alias]
            tui = stack.enter_context(RustainTUI(
                binary=binary, build=False, fresh=not attached, workspace=ws,
                env_overrides=environment, extra_args=["daemon", "attach"] if attached else None,
            ))
            tui.child.logfile_read = stack.enter_context((evidence / (alias + ".terminal.txt")).open("w"))
            assert tui.wait_for_screen("Ready", timeout=30), tui.get_screen_text()
            return tui
        recipient = start_tui("recipient", True)
        sender = start_tui("sender", False)

        def screen(tui, name):
            text = tui.get_screen_text()
            (evidence / (name + ".screen.txt")).write_text(text)
            return text

        def finish(marker):
            def ready():
                text = sender.get_screen_text()
                recipient_text = recipient.get_screen_text()
                if "[y] Allow once" in recipient_text:
                    screen(recipient, marker + "-sender-consent")
                    recipient.send("y")
                return marker in text
            _wait(ready, lambda: screen(sender, marker + "-waiting"), timeout=60)

        # Establish sender consent on the real attached surface before model
        # delegation: its existing input-required path cancels unsupported
        # multi-turn requests instead of waiting for an operator.
        received_ws = worlds["recipient"][0]
        _command(sender, "/team send recipient consent-setup")
        assert recipient.wait_for_screen("[a] Always allow", timeout=60), screen(recipient, "consent-failure")
        screen(recipient, "sender-consent")
        recipient.send("a")
        _wait(lambda: any(row["content"] == "consent-setup"
                          for row in _events(received_ws, "recipient_item_received")),
              lambda: screen(recipient, "consent-waiting"), timeout=60)

        def received_items():
            return [row for row in _events(received_ws, "recipient_item_received")
                    if row["content"] != "consent-setup"]

        _command(sender, "FIRST-DELEGATION")
        assert sender.wait_for_screen("[r] Review", timeout=60), screen(sender, "approval-failure")
        screen(sender, "operator-approval")
        sender.send("s")
        finish("FIRST-COMPLETE")
        _wait(lambda: len(received_items()) == 1,
              lambda: screen(recipient, "first-arrival"))
        _command(sender, "SECOND-DELEGATION")
        finish("SECOND-COMPLETE")
        _wait(lambda: len(received_items()) == 2,
              lambda: screen(recipient, "second-arrival"))
        assert "[r] Review" not in sender.get_screen_text(), "session grant must not prompt again"
        _command(sender, "/team send --type bug_report recipient Session token not refreshed on OAuth→password switch")
        def bug_arrived():
            if "[y] Allow once" in recipient.get_screen_text():
                recipient.send("y")
            return len(received_items()) == 3
        _wait(bug_arrived, lambda: screen(sender, "typed-send-waiting"), timeout=60)
        rows = received_items()
        operator, policy, bug = rows
        assert operator["authorship"] == "agent-composed" and operator["send_authorization"] == "human-decided", operator
        assert policy["authorship"] == "agent-composed" and policy["send_authorization"] == "automatic", policy
        assert bug["authorship"] == "human-written" and bug["send_authorization"] == "human-decided", bug
        (evidence / "received-items.json").write_text(json.dumps(rows, indent=2))

        def item_id(row):
            return row["address"]["item"]

        def view(row, name, required):
            if "[Esc] Close" in recipient.get_screen_text():
                recipient.send("\x1b")
            _command(recipient, "/team view " + item_id(row))
            _wait(lambda: all(token in recipient.get_screen_text() for token in required),
                  lambda: screen(recipient, name + "-waiting"), timeout=60)
            text = screen(recipient, name)
            assert "[truncated]" not in text, text
            return text
        policy_screen = view(policy, "policy", ["[agent-composed]", "[auto-sent]"])
        assert "[agent-composed]" in policy_screen and "[auto-sent]" in policy_screen
        operator_screen = view(operator, "operator", ["[agent-composed]"])
        assert "[auto-sent]" not in operator_screen, operator_screen
        bug_screen = view(bug, "bug-report", ["[a] Acknowledge", "[d] Discuss"])
        assert "[agent-composed]" not in bug_screen and "[auto-sent]" not in bug_screen
        rows = stub.rows()
        assert not any(row.get("desync") for row in rows), rows
        assert not any(row.get("persona") == "recipient" and "turn" in row for row in rows), rows
        assert len([row for row in rows if row.get("persona") == "sender" and "turn" in row]) == 4, rows
