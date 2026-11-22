"""18.10: real same-host QUIC share, durable receiver row, attached 80x24 TUI."""
from contextlib import ExitStack
from datetime import datetime, timezone
import fcntl
import json
import os
from pathlib import Path
import re
import tempfile
import time
import traceback

import pytest

import harness
from fixtures.scene_provider import SceneStub
from harness import (
    StructuralTUI,
    pin_local_fixture_ticket,
    run_fixture_cli,
    send_pty_command,
    sha256_file,
    start_p2p_fixture_daemon,
    stop_process_group,
)


def _seed_sender(workspace, artifact_id):
    journals = list((workspace / ".rustain/rooms").glob("*.jsonl"))
    assert len(journals) == 1, journals
    journal = journals[0]
    with journal.with_suffix(".lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        entries = [json.loads(line) for line in journal.read_text().splitlines() if line]
        tail = entries[-1] if entries else {"seq": 0, "recorded_at_ms": 0}
        event = {"event": "artifact_created", "artifact": {
            "id": artifact_id, "kind": "decision", "producer": "worker",
            "content_hash": artifact_id, "authority": [0] * 32,
            "provenance": ["user_originated"], "depends_on": [], "review": None,
            "host": {"host_id": "sender", "workspace_id": "share-fixture"},
        }}
        entry = {"schema_version": 1, "seq": tail["seq"] + 1,
                 "recorded_at_ms": max(tail["recorded_at_ms"], time.time_ns() // 1_000_000),
                 "record": {"kind": "room", "payload": event}}
        with journal.open("a") as stream:
            stream.write(json.dumps(entry, separators=(",", ":")) + "\n")
            stream.flush()
            os.fsync(stream.fileno())


def _exercise(binary, evidence, root, monkeypatch):
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    scene = evidence / "scene.json"
    scene.write_text(json.dumps({"scene": "18-10-native", "personas": {
        "sender": {"turns": []}, "answerer": {"turns": []}}}) + "\n")
    with ExitStack() as stack:
        stub = stack.enter_context(SceneStub(scene, evidence / "requests.jsonl"))
        worlds = {}
        daemons = {}
        for alias in ("sender", "answerer"):
            ws = root / alias
            config = ws / ".rustain"
            config.mkdir(parents=True)
            (ws / ".env").write_text("")
            home = root / (alias + "-home")
            home.mkdir()
            (config / "p2p.json").write_text('{"listen":true,"agents":{}}\n')
            (config / "relay.json").write_text('{"mode":"disabled"}\n')
            env = {**os.environ, **stub.env(alias, home), "TERM": "xterm-256color",
                   "RUSTAIN_CONFIG_DIR": str(config),
                   "RUSTAIN_DATA_DIR": str(ws / ".rustain_data"),
                   "RUSTAIN_LOG_PATH": str(evidence / (alias + ".runtime.log"))}
            worlds[alias] = (ws, env)
            daemons[alias] = start_p2p_fixture_daemon(stack, binary, worlds[alias], evidence, alias)
        tickets = {}
        for alias, world in worlds.items():
            text = run_fixture_cli(binary, world, ["peer", "invite", "--name", alias], evidence, alias + ".invite")
            tickets[alias] = next(line.strip() for line in text.splitlines() if line.startswith("rustain-peer1."))
        for local, remote in (("sender", "answerer"), ("answerer", "sender")):
            pin_local_fixture_ticket(stack, binary, worlds[local], remote, tickets[remote], evidence)
            for filename in ("p2p.json", "p2p-reach.json"):
                (evidence / (local + "." + filename)).write_bytes(
                    (worlds[local][0] / ".rustain" / filename).read_bytes())
        # The daemon snapshots its dial map at boot. Ticket import updates disk,
        # not that map (DF-18-10-SHARE-DIAL-MAP-SNAPSHOT). Restart the sender
        # before the act; keep the answerer's advertised address live throughout.
        stop_process_group(daemons["sender"])
        daemons["sender"] = start_p2p_fixture_daemon(
            stack, binary, worlds["sender"], evidence, "sender-ready")
        log_args = ["team", "log", "--filter", "kind=topic-received", "--json"]
        before = json.loads(run_fixture_cli(binary, worlds["answerer"], log_args, evidence, "before"))
        assert before["rows"] == [], before
        artifact_id = "a7" * 32
        _seed_sender(worlds["sender"][0], artifact_id)
        shown = run_fixture_cli(binary, worlds["answerer"], ["peer", "show", "sender"], evidence, "sender.identity")
        match = re.search(r"peer id \(sha-256 of that key, hex, whole\)\s+([0-9a-f]+)", shown)
        assert match, shown
        sender_peer = match.group(1)
        ws, env = worlds["answerer"]
        tui = StructuralTUI(binary=binary, build=False, fresh=False, workspace=ws,
                            env_overrides=env, extra_args=["daemon", "attach"])
        tui.screens = stack.enter_context((evidence / "answerer.screens.jsonl").open("w"))
        terminal = stack.enter_context((evidence / "answerer.terminal.txt").open("w"))
        stack.callback(tui.stop)
        tui.start()
        tui.child.delaybeforesend = 0
        tui.child.logfile_read = terminal
        advertised = run_fixture_cli(binary, worlds["sender"], ["peer", "share", "answerer", artifact_id,
            "--topic", "arch-review", "--summary", "event bus decision"], evidence, "share")
        assert "Advertised" in advertised, advertised
        send_pty_command(tui, "/team log --filter=kind=topic-received")
        tui.wait_for_screen("topic-received", timeout=30)
        deadline = time.monotonic() + 30
        while True:
            screen = tui.get_screen_text()
            normalized = "".join(screen.split())
            assert "eventbusdecision" not in normalized, screen
            if "topic-received" in normalized and artifact_id[:12] in normalized:
                break
            assert time.monotonic() < deadline, screen
        (evidence / "topic-received.screen.txt").write_text(screen)
        rows = json.loads(run_fixture_cli(binary, worlds["answerer"], log_args, evidence, "after"))["rows"]
        assert len(rows) == 1, rows
        assert rows[0]["kind"] == "topic-received", rows
        assert rows[0]["peer"] == sender_peer, rows
        assert rows[0]["task"] == "arch-review", rows
        assert artifact_id in rows[0]["summary"], rows
        # A fresh CLI after daemon shutdown reads the same durable receiver state.
        tui.stop()
        stop_process_group(daemons["answerer"])
        offline = json.loads(run_fixture_cli(binary, worlds["answerer"], log_args, evidence, "offline"))["rows"]
        assert offline == rows, (offline, rows)
        assert not any(row.get("turn") or row.get("desync") or row.get("kind") == "desync"
                       for row in stub.rows()), stub.rows()
        (evidence / "assertions.txt").write_text(
            "PASS: reciprocal tickets and real foreground QUIC listeners.\n"
            "PASS: no receiver receipt before share; real peer share exits 0 with Advertised.\n"
            "PASS: attached 80x24 /team log shows topic-received and artifact prefix, not summary prose.\n"
            "PASS: receiver JSON row names full sender PeerId, topic and artifact id.\n"
            "PASS: same row remains readable from disk with receiver daemon stopped.\n")


@pytest.mark.p2p_binary
def test_topic_share_leaves_a_durable_receiver_row(monkeypatch):
    root = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())).resolve()
    root.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="18-10-", dir=root))
    manifest = {"run_id": evidence.name, "started_at": datetime.now(timezone.utc).isoformat(),
                "evidence_dir": str(evidence), "result": "RUNNING", "viewport": [80, 24],
                "binary": os.environ.get("RUSTAIN_TUI_BINARY")}
    identity = evidence / "run.json"
    identity.write_text(json.dumps(manifest, indent=2) + "\n")
    try:
        binary = Path(os.environ["RUSTAIN_TUI_BINARY"]).resolve()
        assert binary.is_file(), binary
        assert "release" not in str(binary), "this smoke requires a debug p2p binary"
        manifest.update({"binary": str(binary), "binary_sha256": sha256_file(binary),
                         "binary_bytes": binary.stat().st_size})
        identity.write_text(json.dumps(manifest, indent=2) + "\n")
        with tempfile.TemporaryDirectory(prefix="r1810-", dir="/tmp") as workspace:
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
