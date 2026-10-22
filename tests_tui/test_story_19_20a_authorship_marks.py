"""AC4 native key-path smoke; real sender, attached answerer, no receipt wiring."""
from contextlib import ExitStack
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import traceback
import urllib.request
import urllib.error

import pytest
import harness
from harness import RustainTUI
from fixtures.scene_provider import SceneStub
from test_story_19_19_act2_consultation import _command, _events, _stop, _wait

EVIDENCE_ROOT = Path(__file__).resolve().parents[2] / "_bmad-output/implementation-artifacts/19-20a-native-smoke"
AUTO = "Received. Reviewing."
APPROVED = "Consultation reviewed: preserve publication ordering."


def _sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


@pytest.mark.a2a_binary
@pytest.mark.timeout(240)
def test_authorship_marks_and_older_auto_retraction(monkeypatch):
    evidence_root = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", EVIDENCE_ROOT)).resolve()
    evidence_root.mkdir(parents=True, exist_ok=True)
    started = datetime.now(timezone.utc).isoformat()
    evidence = Path(tempfile.mkdtemp(prefix="19-20a-", dir=evidence_root))
    manifest = {"run_id": evidence.name, "started_at": started,
                "evidence_dir": str(evidence), "result": "RUNNING",
                "binary": os.environ.get("RUSTAIN_TUI_BINARY")}
    identity = evidence / "run.json"
    identity.write_text(json.dumps(manifest, indent=2) + "\n")
    try:
        binary = Path(os.environ["RUSTAIN_TUI_BINARY"]).resolve()
        manifest["binary"] = str(binary)
        assert binary.is_file(), binary
        manifest.update({"binary_sha256": _sha256(binary),
                         "binary_bytes": binary.stat().st_size})
        identity.write_text(json.dumps(manifest, indent=2) + "\n")
        # Keep production AF_UNIX sockets below SUN_LEN, and remove private
        # workspaces/homes only after all daemon and PTY contexts have exited.
        with tempfile.TemporaryDirectory(prefix="r20a-", dir="/tmp") as workspace:
            _exercise_authorship_marks(binary, evidence, Path(workspace), monkeypatch)
        assert _sha256(binary) == manifest["binary_sha256"], "native binary changed during capture"
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
        (evidence / "smoke-result.txt").write_text(
            manifest["result"] + "\n" + manifest.get("failure", ""))
        manifest["captures"] = {
            path.name: {"sha256": _sha256(path), "bytes": path.stat().st_size}
            for path in sorted(evidence.iterdir()) if path.is_file() and path != identity
        }
        identity.write_text(json.dumps(manifest, indent=2) + "\n")


def _exercise_authorship_marks(binary, evidence, tmp_path, monkeypatch):
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    scene = evidence / "scene.json"
    scene.write_text(json.dumps({"scene": "19-20a-native", "personas": {
        "answerer": {"turns": [{"expect": "CONSULTATION-19-20A", "content": [
            {"type": "text", "text": APPROVED}], "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 10}}]},
        "sender": {"turns": []}}}))
    with ExitStack() as stack:
        stub = stack.enter_context(SceneStub(scene, evidence / "requests.jsonl"))
        worlds = {}
        reservations = {}
        for alias in ("sender", "answerer"):
            ws = tmp_path / alias
            (ws / ".rustain").mkdir(parents=True)
            (ws / ".env").write_text("")
            home = tmp_path / (alias + "-home")
            home.mkdir()
            reservation = socket.socket()
            reservation.bind(("127.0.0.1", 0))
            reservations[alias] = reservation
            stack.callback(reservation.close)
            env = {**os.environ, **stub.env(alias, home), "TERM": "xterm-256color",
                   "RUSTAIN_CONFIG_DIR": str(ws / ".rustain"),
                   "RUSTAIN_DATA_DIR": str(ws / ".rustain_data"),
                   "RUSTAIN_LOG_PATH": str(evidence / (alias + ".runtime.log"))}
            worlds[alias] = (ws, env, reservation.getsockname()[1])
        for alias, (ws, env, port) in worlds.items():
            peers = {peer: {"url": f"http://127.0.0.1:{world[2]}/"}
                     for peer, world in worlds.items() if peer != alias}
            (ws / ".rustain/a2a.json").write_text(json.dumps({"server": {"admission": "allow"}, "agents": peers}))
            (ws / ".rustain/a2a-interaction.toml").write_text(
                '[interaction.defaults]\nresponse_mode = "notify-and-auto"\nnotification = "immediate"\n')
            if alias == "answerer":
                (ws / ".rustain/team-policy.toml").write_text(
                    '[team.overrides.consultation]\nresponse_mode = "notify-and-draft"\n')
            reservations[alias].close()
            log = stack.enter_context((evidence / (alias + ".daemon.log")).open("w"))
            process = subprocess.Popen([str(binary), f"--serve-a2a=127.0.0.1:{port}",
                "daemon", "start", "--foreground"], cwd=ws, env=env,
                stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            stack.callback(_stop, process)
            def ready():
                assert process.poll() is None, "daemon exited before agent card"
                try:
                    with urllib.request.urlopen(
                            f"http://127.0.0.1:{port}/.well-known/agent-card.json",
                            timeout=1) as response:
                        return response.status == 200
                except (OSError, urllib.error.URLError):
                    return False
            _wait(ready, lambda: "Awaiting product agent card")
        def tui(alias, attached):
            ws, env, _ = worlds[alias]
            session = stack.enter_context(RustainTUI(binary=binary, build=False,
                fresh=not attached, workspace=ws, env_overrides=env,
                extra_args=["daemon", "attach"] if attached else None))
            session.child.logfile_read = stack.enter_context((evidence / (alias + ".terminal.txt")).open("w"))
            assert session.wait_for_screen("Ready", timeout=30), session.get_screen_text()
            return session
        answerer = tui("answerer", True)
        sender = tui("sender", False)
        def save(name):
            text = answerer.get_screen_text()
            (evidence / (name + ".screen.txt")).write_text(text)
            return text
        def wait(text, name):
            assert answerer.wait_for_screen(text, timeout=60), save(name + "-failure")
            return save(name)
        def span(body, name):
            # A short response fits completely in one viewport. Scan real chat
            # pages until its role, body, marker (if any), and next row boundary
            # are painted together; never infer a missing tag from clipping.
            # The offset is not clamped when paging above the oldest row.
            # Reset to the newest row before each independent bounded scan.
            for _ in range(100):
                answerer.send("\x1b[6~")
            for _ in range(16):
                answerer.send("\x1b[5~")
            time.sleep(.15)
            for page in range(32):
                text = save(f"{name}-{page:02}")
                rows = text.splitlines()
                for i, row in enumerate(rows):
                    if body in row:
                        start = next((j for j in range(i - 1, -1, -1)
                                      if "Assistant:" in rows[j]), None)
                        if start is not None:
                            end = next((j for j in range(i + 1, len(rows))
                                        if not rows[j].strip() or "Assistant:" in rows[j]
                                        or "System:" in rows[j] or "You:" in rows[j]), None)
                            if end is not None:
                                result = "\n".join(rows[start:end])
                                (evidence / (name + ".span.txt")).write_text(result)
                                return result
                answerer.send("\x1b[6~")
                time.sleep(.08)
            pytest.fail("Complete rendered response span not found: " + body)
        _command(sender, "/team send --type status_request answerer STATUS-19-20A")
        _wait(lambda: any(label in answerer.get_screen_text()
                         for label in ("[y] Allow once", "[auto-sent]")),
              lambda: save("status-arrival-failure"), timeout=60)
        if "[y] Allow once" in answerer.get_screen_text():
            save("sender-consent")
            answerer.send("y")
        wait("[auto-sent]", "older-auto")
        auto_before = span(AUTO, "auto-before")
        assert auto_before.count("[agent-composed]") == 1, auto_before
        assert "[auto-sent]" in auto_before and "[✗] Retract" in auto_before, auto_before
        _command(sender, "/team send --type consultation answerer CONSULTATION-19-20A")
        _wait(lambda: any(label in answerer.get_screen_text()
                         for label in ("[y] Allow once", "[y] Approve")),
              lambda: save("consultation-arrival-failure"), timeout=60)
        if "[y] Allow once" in answerer.get_screen_text():
            save("consultation-consent")
            answerer.send("y")
        wait("Peer draft:", "draft-ready")
        answerer.send("\t")
        wait("[y] Approve", "draft-card")
        answerer.send("y")
        _wait(lambda: "[y] Approve" not in answerer.get_screen_text(), answerer.get_screen_text)
        wait("[Esc] Close", "approved-item-actions")
        answerer.send("\x1b")
        save("approved-newer")
        approved_before = span(APPROVED, "approved-before")
        assert approved_before.count("[agent-composed]") == 1, approved_before
        assert "[auto-sent]" not in approved_before and "[✗] Retract" not in approved_before, approved_before
        # Typed sends are pull-only. Read their actual remote tasks to terminal
        # before retracting (Hazard 6); this is observation, not a fabricated
        # disclosure or a second send path.
        for index, received in enumerate(_events(worlds["answerer"][0], "recipient_item_received")):
            def terminal():
                request = urllib.request.Request(
                    f"http://127.0.0.1:{worlds['answerer'][2]}/",
                    data=json.dumps({"jsonrpc": "2.0", "id": index,
                        "method": "tasks/get", "params": {"id": received["task"]}}).encode(),
                    headers={"Content-Type": "application/json"})
                with urllib.request.urlopen(request, timeout=5) as response:
                    result = json.load(response)
                (evidence / f"task-{index}.json").write_text(json.dumps(result, indent=2))
                assert "error" not in result, result
                return result if result["result"]["status"]["state"] == "completed" else None
            _wait(terminal, lambda: "Remote task did not reach product-minted completed state")
        answerer.send("\x18")
        preview = wait("Retract this auto-sent message?", "retract-preview")
        assert AUTO in preview and APPROVED not in preview.split("Retract this auto-sent message?", 1)[1], preview
        for card in ("[y] Approve", "[y] Allow", "[r] Review", "sender consent", "[y] Send"):
            assert card not in preview, preview
        answerer.send("y")
        wait("Retracted. Marked in your log", "retracted")
        auto_after = span(AUTO, "auto-after")
        assert "[retracted" in auto_after and "[✗] Retract (used)" in auto_after, auto_after
        assert AUTO in auto_after, auto_after
        approved_after = span(APPROVED, "approved-after")
        assert approved_after == approved_before, (approved_before, approved_after)
        (evidence / "assertions.txt").write_text(
            "PASS: auto span has both provenance marks; approved newer span has exactly one agent-composed mark and no auto/retract mark.\n"
            "PASS: Ctrl+X preview selects older auto body; no competing approval/consent card before y.\n"
            "PASS: auto body preserved with retracted and consumed-retract marks; approved complete span unchanged.\n")
