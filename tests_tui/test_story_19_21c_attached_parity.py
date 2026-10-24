"""Story 19.21c: one debug-native 80x24 attached thread/followup/retract journey."""
from contextlib import ExitStack
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import traceback
import urllib.error
import urllib.request

import pytest
import harness
from fixtures.scene_provider import SceneStub
from test_story_19_20b_rap_one_way import _StructuralTUI, _sha256, _stop
from test_story_19_21_act1_distribution import _command, _events


def _wait_screen(tui, predicate, label, timeout=45):
    deadline = time.monotonic() + timeout
    while True:
        screen = tui.get_screen_text()  # blocks on actual PTY output
        assert "unavailable in this session" not in screen, screen
        if predicate(screen):
            return screen
        assert time.monotonic() < deadline, f"{label}:\n{screen}"


def _exercise(binary, evidence, root, monkeypatch):
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    scene = evidence / "scene.json"
    scene.write_text(json.dumps({"scene": "19-21c-native", "personas": {
        "A": {"turns": []}, "B": {"turns": []}}}) + "\n")
    with ExitStack() as stack:
        stub = stack.enter_context(SceneStub(scene, evidence / "requests.jsonl"))
        worlds, reservations, endpoints = {}, {}, {}
        for alias in ("A", "B"):
            reservation = socket.socket()
            reservation.bind(("127.0.0.1", 0))
            reservations[alias] = reservation
            stack.callback(reservation.close)
            endpoints[alias] = f"http://127.0.0.1:{reservation.getsockname()[1]}/"
        for alias in ("A", "B"):
            ws = root / alias
            config = ws / ".rustain"
            config.mkdir(parents=True)
            (ws / ".env").write_text("")
            home = root / (alias + "-home")
            home.mkdir()
            (config / "a2a.json").write_text(json.dumps({
                "server": {"admission": "allow"},
                "agents": {other: {"url": endpoint} for other, endpoint in endpoints.items() if other != alias},
            }) + "\n")
            (config / "a2a-interaction.toml").write_text(
                '[interaction.defaults]\nresponse_mode = "notify-and-wait"\nnotification = "immediate"\n')
            (config / "relay.json").write_text('{"mode":"disabled"}\n')
            env = {**os.environ, **stub.env(alias, home), "TERM": "xterm-256color",
                   "RUSTAIN_CONFIG_DIR": str(config), "RUSTAIN_DATA_DIR": str(ws / ".rustain_data"),
                   "RUSTAIN_LOG_PATH": str(evidence / (alias + ".runtime.log"))}
            worlds[alias] = (ws, env)
        for alias in ("B", "A"):
            ws, env = worlds[alias]
            port = reservations[alias].getsockname()[1]
            reservations[alias].close()
            logfile = evidence / (alias + ".daemon.log")
            log = stack.enter_context(logfile.open("w"))
            process = subprocess.Popen([str(binary), f"--serve-a2a=127.0.0.1:{port}", "daemon", "start", "--foreground"],
                                       cwd=ws, env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            stack.callback(_stop, process)
            deadline = time.monotonic() + 45
            while True:
                assert process.poll() is None, logfile.read_text()
                status = subprocess.run([str(binary), "daemon", "status"], cwd=ws, env=env,
                                        capture_output=True, text=True, timeout=5)
                try:
                    with urllib.request.urlopen(endpoints[alias] + ".well-known/agent-card.json", timeout=1) as response:
                        served = response.status == 200
                except (OSError, urllib.error.URLError):
                    served = False
                if served and status.returncode == 0 and "running" in (status.stdout + status.stderr).lower():
                    break
                assert time.monotonic() < deadline, logfile.read_text() + status.stdout + status.stderr
        # Admission and sender consent are independent. Establish standing consent
        # through the recipient's real attached writer before the parity journey.
        ws, env = worlds["B"]
        recipient = _StructuralTUI(binary=binary, build=False, fresh=False, workspace=ws,
                                   env_overrides=env, extra_args=["daemon", "attach"])
        recipient.screens = stack.enter_context((evidence / "B.screens.jsonl").open("w"))
        recipient_log = stack.enter_context((evidence / "B.terminal.txt").open("w"))
        stack.callback(recipient.stop)
        recipient.start()
        recipient.child.delaybeforesend = 0
        recipient.child.logfile_read = recipient_log
        ws, env = worlds["A"]
        tui = _StructuralTUI(binary=binary, build=False, fresh=False, workspace=ws,
                             env_overrides=env, extra_args=["daemon", "attach"])
        tui.screens = stack.enter_context((evidence / "A.screens.jsonl").open("w"))
        terminal_log = stack.enter_context((evidence / "A.terminal.txt").open("w"))
        stack.callback(tui.stop)
        tui.start()
        tui.child.delaybeforesend = 0
        tui.child.logfile_read = terminal_log
        _command(tui, "/team send B consent-setup")
        _wait_screen(recipient, lambda screen: "[a] Always allow" in screen, "recipient sender consent")
        recipient.send("a")
        _wait_screen(recipient, lambda screen: "consent-setup" in screen and "Always allow" not in screen,
                     "recipient accepted setup")
        _command(tui, "/team send B hello")
        screen = _wait_screen(tui, lambda screen: "delivered" in screen and "●" in screen, "first send")
        (evidence / "01-send.screen.txt").write_text(screen)
        received = [event for event in _events(worlds["B"][0], "recipient_item_received")
                    if event["content"] != "consent-setup"]
        assert len(received) == 1, received
        item = received[0]["address"]["item"]
        _command(tui, "/team board B")
        screen = _wait_screen(tui, lambda screen: item in screen, "board root")
        (evidence / "02-board.screen.txt").write_text(screen)
        _command(tui, f"/team thread B {item}")
        screen = _wait_screen(tui, lambda screen: f"thread · B · {item}" in screen, "thread view")
        (evidence / "03-thread.screen.txt").write_text(screen)
        _command(tui, f"/team followup B {item} more")
        screen = _wait_screen(tui, lambda screen: screen.count("delivered") >= 3 and "sending…" not in screen,
                              "second delivered receipt")
        (evidence / "04-followup.screen.txt").write_text(screen)
        received = [event for event in _events(worlds["B"][0], "recipient_item_received")
                    if event["content"] != "consent-setup"]
        assert len(received) == 2, received
        assert received[1]["in_reply_to"] == item, received
        _command(tui, f"/team retract B {item}")
        screen = _wait_screen(tui, lambda screen: "Retract on B's host" in screen and "[y] Retract" in screen,
                              "armed retract card")
        (evidence / "05-card.screen.txt").write_text(screen)
        tui.send("y")
        screen = _wait_screen(tui, lambda screen: "Retracted on B's host." in screen, "landed retract")
        (evidence / "06-landed.screen.txt").write_text(screen)
        marks = _events(worlds["B"][0], "recipient_item_retracted")
        assert len(marks) == 1, marks
        assert not any(row.get("turn") is not None or row.get("desync") for row in stub.rows()), stub.rows()
        for line in (evidence / "A.screens.jsonl").read_text().splitlines():
            assert "unavailable in this session" not in json.loads(line)["screen"]
        for alias, (workspace, _) in worlds.items():
            rows = {path.name: path.read_text() for path in (workspace / ".rustain/rooms").glob("*.jsonl")}
            (evidence / (alias + ".journal.json")).write_text(json.dumps(rows, indent=2) + "\n")
        (evidence / "assertions.txt").write_text(
            "PASS: debug attached writer, 80x24; recipient standing sender consent via native card.\n"
            "PASS: send, board, thread, linked followup, armed card and landed retract visible.\n"
            "PASS: two journey items plus consent setup, reply link, one recipient mark, zero provider turns, no S7.\n")


@pytest.mark.a2a_binary
@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(240)
def test_attached_thread_followup_retract_80x24(monkeypatch):
    evidence_root = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())).resolve()
    evidence_root.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="19-21c-", dir=evidence_root))
    manifest = {"run_id": evidence.name, "started_at": datetime.now(timezone.utc).isoformat(),
                "evidence_dir": str(evidence), "viewport": [80, 24], "result": "RUNNING"}
    identity = evidence / "run.json"
    try:
        binary = Path(os.environ["RUSTAIN_TUI_BINARY"]).resolve()
        assert binary.is_file() and "release" not in binary.parts, binary
        manifest.update(binary=str(binary), binary_sha256=_sha256(binary))
        identity.write_text(json.dumps(manifest, indent=2) + "\n")
        with tempfile.TemporaryDirectory(prefix="r19-21c-", dir="/tmp") as root:
            _exercise(binary, evidence, Path(root), monkeypatch)
        assert _sha256(binary) == manifest["binary_sha256"], "binary changed during native smoke"
        manifest["result"] = "PASS"
    except BaseException:
        manifest["result"] = "FAIL"
        (evidence / "failure.txt").write_text(traceback.format_exc())
        raise
    finally:
        manifest["completed_at"] = datetime.now(timezone.utc).isoformat()
        identity.write_text(json.dumps(manifest, indent=2) + "\n")
        print(f"NATIVE_19_21C_EVIDENCE={evidence}", flush=True)
