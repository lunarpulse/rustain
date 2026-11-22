"""19.20b Task 6: real RAP ping, consent and attached 80x24 terminal.

Supersedes 19.20a's RAP auto-template smoke: RAP now paints a one-way
notice, not an automatic reply. Only provider boot probes are stubbed;
identities, tickets, admission, QUIC, verdict and terminal are the product.
"""
from contextlib import ExitStack
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time
import traceback

import pytest

import harness
from fixtures.scene_provider import SceneStub
from decision_frame_helpers import open_decision
from harness import (
    StructuralTUI,
    pin_local_fixture_ticket,
    run_fixture_cli,
    send_pty_command,
    sha256_file,
    start_p2p_fixture_daemon,
    stop_process_group,
    wait_for_frame,
)

NOTICE = "[no reply possible]"
FORBIDDEN = "Received. Reviewing."
TEAM_LOG_CLAIM = "append-only,crash-safe,structurallyreplayable"
TEAM_LOG_RAP_ROW = re.compile(
    r"\d{4}-\d{2}-\d{2}\d{2}:\d{2}:\d{2}Z←inbound✓acceptedviaRAP·peer[0-9a-f]+·task[^·]+"
    r"·messagedeliveredoneway;notaskrunsandnothingissentback"
)


def _compact(screen):
    return "".join(ch for ch in screen if not ch.isspace() and ch not in "│┆")


def _guard_rap_paint(screen):
    compact = _compact(screen)
    assert _compact(FORBIDDEN) not in compact, screen
    assert "taskacceptedandexecuting" not in compact, screen


def _read_team_log(tui, evidence):
    send_pty_command(tui, "/team log")

    def shows_rap_log(screen):
        compact = _compact(screen)
        return TEAM_LOG_CLAIM in compact and TEAM_LOG_RAP_ROW.search(compact)

    screen = wait_for_frame(tui, shows_rap_log, "timestamped one-way RAP team log", timeout=30)
    (evidence / "team-log.screen.txt").write_text(screen)
    with (evidence / "assertions.txt").open("a") as stream:
        stream.write("PASS: /team log at 80x24 shows the timestamped accepted-via-RAP row, "
                     "one-way summary and structural-replay claim.\n")


def _exercise(binary, evidence, root, monkeypatch):
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    scene = evidence / "scene.json"
    scene.write_text(json.dumps({"scene": "19-20b-native", "personas": {
        "sender": {"turns": []}, "answerer": {"turns": []}}}) + "\n")
    with ExitStack() as stack:
        stub = stack.enter_context(SceneStub(scene, evidence / "requests.jsonl"))
        worlds = {}
        for alias in ("sender", "answerer"):
            ws = root / alias
            config = ws / ".rustain"
            config.mkdir(parents=True)
            (ws / ".env").write_text("")
            home = root / (alias + "-home")
            home.mkdir()
            (config / "p2p.json").write_text(json.dumps({"listen": True, "agents": {}}) + "\n")
            (config / "relay.json").write_text('{"mode":"disabled"}\n')
            (config / "a2a-interaction.toml").write_text(
                '[interaction.defaults]\nresponse_mode = "notify-and-auto"\nnotification = "immediate"\n')
            env = {**os.environ, **stub.env(alias, home), "TERM": "xterm-256color",
                   "RUSTAIN_CONFIG_DIR": str(config),
                   "RUSTAIN_DATA_DIR": str(ws / ".rustain_data"),
                   "RUSTAIN_LOG_PATH": str(evidence / (alias + ".runtime.log"))}
            worlds[alias] = (ws, env)
            start_p2p_fixture_daemon(stack, binary, worlds[alias], evidence, alias)

        tickets = {}
        for alias, world in worlds.items():
            output = run_fixture_cli(binary, world, ["peer", "invite", "--name", alias],
                                     evidence, alias + ".invite")
            tickets[alias] = next(line.strip() for line in output.splitlines()
                                  if line.startswith("rustain-peer1."))
            assert "carries no network address" not in output, output
        for local, remote in (("sender", "answerer"), ("answerer", "sender")):
            pin_local_fixture_ticket(stack, binary, worlds[local], remote, tickets[remote], evidence)
            config = worlds[local][0] / ".rustain"
            for filename in ("p2p.json", "p2p-reach.json", "a2a-interaction.toml"):
                (evidence / (local + "." + filename)).write_bytes((config / filename).read_bytes())

        ws, env = worlds["answerer"]
        # Register cleanup before start so failed startup also closes the PTY.
        tui = StructuralTUI(binary=binary, build=False, fresh=False, workspace=ws,
                            env_overrides=env, extra_args=["daemon", "attach"],
                            frame_guard=_guard_rap_paint)
        tui.screens = stack.enter_context((evidence / "answerer.screens.jsonl").open("w"))
        terminal_log = stack.enter_context((evidence / "answerer.terminal.txt").open("w"))
        stack.callback(tui.stop)
        tui.start()
        tui.child.delaybeforesend = 0
        tui.child.logfile_read = terminal_log
        (evidence / "ready.screen.txt").write_text(tui.get_screen_text())

        sender_ws, sender_env = worlds["sender"]
        ping_log = stack.enter_context((evidence / "sender.ping.txt").open("w"))
        ping = subprocess.Popen([str(binary), "peer", "ping", "answerer"],
                                cwd=sender_ws, env=sender_env, stdout=ping_log,
                                stderr=subprocess.STDOUT, start_new_session=True)
        stack.callback(stop_process_group, ping)
        deadline = time.monotonic() + 60
        consent_answered = False
        while True:
            screen = tui.get_screen_text()
            if NOTICE in screen:
                break
            if not consent_answered and ("[y] Allow once" in screen or
                                         ("Needs you (" in screen and "^X q" in screen)):
                screen = open_decision(tui, "Peer consent", ("[y] Allow once",), timeout=60)
                (evidence / "consent.screen.txt").write_text(screen)
                tui.send("y")
                consent_answered = True
            assert ping.poll() in (None, 0), (evidence / "sender.ping.txt").read_text()
            assert time.monotonic() < deadline, screen + (evidence / "sender.ping.txt").read_text()
        assert ping.wait(timeout=15) == 0, (evidence / "sender.ping.txt").read_text()
        verdict = (evidence / "sender.ping.txt").read_text()
        assert "— accepted." in verdict, verdict
        final = tui.get_screen_text()
        assert NOTICE in final, final
        (evidence / "one-way.screen.txt").write_text(final)
        (evidence / "assertions.txt").write_text(
            "PASS: real reciprocal tickets/rosters and foreground p2p listeners.\n"
            "PASS: sender peer ping returned accepted.\n"
            "PASS: attached 80x24 screen shows [no reply possible].\n"
            "PASS: no observed screen claims auto-response or task execution.\n"
            f"Consent card answered: {consent_answered}\n")
        _read_team_log(tui, evidence)
        observed = stub.rows()
        assert not any(row.get("turn") or row.get("desync") or row.get("kind") == "desync"
                       for row in observed), observed
        with (evidence / "assertions.txt").open("a") as stream:
            stream.write("PASS: no provider turn or desync through the /team log observation.\n")


@pytest.mark.a2a_binary
def test_rap_ping_is_one_way_on_attached_80x24_terminal(monkeypatch):
    root = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())).resolve()
    root.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="19-20b-", dir=root))
    manifest = {"run_id": evidence.name, "started_at": datetime.now(timezone.utc).isoformat(),
                "evidence_dir": str(evidence), "result": "RUNNING", "viewport": [80, 24],
                "binary": os.environ.get("RUSTAIN_TUI_BINARY")}
    identity = evidence / "run.json"
    identity.write_text(json.dumps(manifest, indent=2) + "\n")
    try:
        binary = Path(os.environ["RUSTAIN_TUI_BINARY"]).resolve()
        assert binary.is_file(), binary
        assert "release" not in binary.parts, "this smoke requires a debug a2a,p2p binary"
        manifest.update({"binary": str(binary), "binary_sha256": sha256_file(binary),
                         "binary_bytes": binary.stat().st_size})
        identity.write_text(json.dumps(manifest, indent=2) + "\n")
        # Short scratch paths keep the real AF_UNIX attach sockets below SUN_LEN.
        with tempfile.TemporaryDirectory(prefix="r20b-", dir="/tmp") as workspace:
            _exercise(binary, evidence, Path(workspace), monkeypatch)
        assert sha256_file(binary) == manifest["binary_sha256"], "binary changed during capture"
    except BaseException:
        manifest.update({"result": "FAIL", "failure": traceback.format_exc()})
        (evidence / "assertions.txt").write_text("FAIL: native smoke did not complete.\n")
        raise
    else:
        manifest["result"] = "PASS"
    finally:
        manifest["finished_at"] = datetime.now(timezone.utc).isoformat()
        (evidence / "smoke-binary.txt").write_text(json.dumps({
            key: manifest.get(key) for key in ("binary", "binary_sha256", "binary_bytes")
        }, indent=2) + "\n")
        (evidence / "smoke-result.txt").write_text(manifest["result"] + "\n" + manifest.get("failure", ""))
        manifest["captures"] = {
            path.name: {"sha256": sha256_file(path), "bytes": path.stat().st_size}
            for path in sorted(evidence.iterdir()) if path.is_file() and path != identity}
        identity.write_text(json.dumps(manifest, indent=2) + "\n")
