"""Story 19.22: debug-native Act 6 list/verdict journey at 80x24."""
import base64
from contextlib import ExitStack
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import socket
import ssl
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
from test_story_19_21c_attached_parity import _wait_screen


def _exercise(binary, evidence, root, monkeypatch):
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    scene = evidence / "scene.json"
    scene.write_text(json.dumps({"scene": "19-22-native", "personas": {
        "A": {"turns": []}, "B": {"turns": []}}}) + "\n")
    # RFC8032 public test-vector identity, matching the established native fixture.
    seed = bytes.fromhex("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60")
    public = bytes.fromhex("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a")
    sender_pin = {"alg": "EdDSA", "x": base64.urlsafe_b64encode(public).decode().rstrip("=")}
    with ExitStack() as stack:
        stub = stack.enter_context(SceneStub(scene, evidence / "requests.jsonl"))
        worlds, reservations, endpoints = {}, {}, {}
        for alias in ("A", "B"):
            reservation = socket.socket()
            reservation.bind(("127.0.0.1", 0))
            reservations[alias] = reservation
            stack.callback(reservation.close)
            endpoints[alias] = f"http://127.0.0.1:{reservation.getsockname()[1]}/"
        endpoints["B"] = f"https://localhost:{reservations['B'].getsockname()[1]}/"
        cert, tls_key = root / "recipient-leaf.pem", root / "recipient-leaf.key"
        subprocess.run([
            "openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256",
            "-nodes", "-keyout", str(tls_key), "-out", str(cert), "-days", "1", "-subj", "/CN=localhost",
            "-addext", "basicConstraints=critical,CA:FALSE", "-addext", "subjectAltName=DNS:localhost",
            "-addext", "extendedKeyUsage=serverAuth",
        ], check=True, capture_output=True)
        tls_context = ssl.create_default_context(cafile=str(cert))
        for alias in ("A", "B"):
            ws = root / alias
            config = ws / ".rustain"
            config.mkdir(parents=True)
            (ws / ".env").write_text("")
            home = root / (alias + "-home")
            home.mkdir()
            server = {"admission": "allow"}
            agents = {other: {"url": endpoint} for other, endpoint in endpoints.items() if other != alias}
            if alias == "A":
                agents["B"]["auth"] = "NATIVE_A_TO_B_KEY"
                agents["B"]["caCert"] = str(cert)
                data = ws / ".rustain_data"
                data.mkdir()
                key = data / "peer-identity.pkcs8.der"
                key.write_bytes(bytes.fromhex("302e020100300506032b657004220420") + seed)
                key.chmod(0o600)
            else:
                agents["A"]["pinnedKey"] = sender_pin
                server["peerKeys"] = {"NATIVE_A_TO_B_KEY": "A"}
                server["tls"] = {"cert": str(cert), "key": str(tls_key)}
                server["advertisedHost"] = f"localhost:{reservations['B'].getsockname()[1]}"
            (config / "a2a.json").write_text(json.dumps({"server": server, "agents": agents}) + "\n")
            (config / "a2a-interaction.toml").write_text(
                '[interaction.defaults]\nresponse_mode = "notify-and-wait"\nnotification = "immediate"\n')
            (config / "relay.json").write_text('{"mode":"disabled"}\n')
            env = {**os.environ, **stub.env(alias, home), "TERM": "xterm-256color",
                   "RUSTAIN_CONFIG_DIR": str(config), "RUSTAIN_DATA_DIR": str(ws / ".rustain_data"),
                   "RUSTAIN_LOG_PATH": str(evidence / (alias + ".runtime.log")),
                   "NATIVE_A_TO_B_KEY": "story-19-22-native-test-key"}
            worlds[alias] = (ws, env)
        for alias in ("B", "A"):
            ws, env = worlds[alias]
            port = reservations[alias].getsockname()[1]
            reservations[alias].close()
            logfile = evidence / (alias + ".daemon.log")
            log = stack.enter_context(logfile.open("w"))
            bind = "0.0.0.0" if alias == "B" else "127.0.0.1"
            process = subprocess.Popen([str(binary), f"--serve-a2a={bind}:{port}", "daemon", "start", "--foreground"],
                                       cwd=ws, env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            stack.callback(_stop, process)
            deadline = time.monotonic() + 45
            while True:
                assert process.poll() is None, logfile.read_text()
                status = subprocess.run([str(binary), "daemon", "status"], cwd=ws, env=env,
                                        capture_output=True, text=True, timeout=5)
                try:
                    with urllib.request.urlopen(endpoints[alias] + ".well-known/agent-card.json", timeout=1,
                                                context=tls_context if alias == "B" else None) as response:
                        served = response.status == 200
                except (OSError, urllib.error.URLError):
                    served = False
                if served and status.returncode == 0 and "running" in (status.stdout + status.stderr).lower():
                    break
                assert time.monotonic() < deadline, logfile.read_text() + status.stdout + status.stderr
        clients = {}
        for alias in ("B", "A"):
            ws, env = worlds[alias]
            tui = _StructuralTUI(binary=binary, build=False, fresh=False, workspace=ws,
                                 env_overrides=env, extra_args=["daemon", "attach"])
            tui.screens = stack.enter_context((evidence / (alias + ".screens.jsonl")).open("w"))
            terminal_log = stack.enter_context((evidence / (alias + ".terminal.txt")).open("w"))
            stack.callback(tui.stop)
            tui.start()
            tui.child.delaybeforesend = 0
            tui.child.logfile_read = terminal_log
            clients[alias] = tui
        sender, recipient = clients["A"], clients["B"]
        _command(sender, "/team send B consent-setup")
        _wait_screen(recipient, lambda screen: "[a] Always allow" in screen, "standing sender consent")
        recipient.send("a")
        _wait_screen(recipient, lambda screen: "consent-setup" in screen and "Always allow" not in screen,
                     "recipient accepted setup")
        setup = [row for row in _events(worlds["B"][0], "recipient_item_received") if row["content"] == "consent-setup"]
        assert len(setup) == 1, setup
        setup_item = setup[0]["address"]["item"]
        _command(recipient, f"/team reply {setup_item} setup-complete")
        _wait_screen(recipient, lambda screen: "reply recorded" in screen.lower() and "[Tab] Item actions" not in screen,
                     "setup item resolved before assignment card")

        _command(sender, "/team send --type story_assignment --stories S7-09=B B S7-09: Fix payment timeout")
        screen = _wait_screen(recipient, lambda screen: "story_assignment · A" in screen and "1 story, 1 assigned to you" in screen,
                              "assignment arrival summary")
        (evidence / "01-assignment-card.screen.txt").write_text(screen)
        recipient.send("\t")
        screen = _wait_screen(recipient, lambda screen: "[y] Accept" in screen and "[d] Discuss concerns" in screen,
                              "typed verdict keys on Item surface")
        (evidence / "02-verdict-actions.screen.txt").write_text(screen)
        recipient.send("y")
        recipient.send("\x1b")
        screen = _wait_screen(recipient, lambda screen: "Accepted." in screen and "[Tab] Item actions" not in screen,
                              "Accept settles the pending card")
        (evidence / "03-accepted.screen.txt").write_text(screen)
        turns = [row for row in _events(worlds["B"][0], "recipient_item_replied")
                 if row.get("kind", {}).get("type") == "accept"]
        assert len(turns) == 1 and turns[0]["content"] == "Accepted." and turns[0].get("via_node"), turns
        assert turns[0]["authorship"] == "human-written", turns
        assert turns[0]["send_authorization"] == "human-decided", turns

        _command(sender, "/team send --type scope_change --stories S7-09=B,S7-07 B Sprint 7 scope change")
        _wait_screen(recipient, lambda screen: "scope_change · A" in screen and "2 stories, 1 assigned to you" in screen,
                     "scope-change arrival summary")
        recipient.send("\t")
        screen = _wait_screen(recipient, lambda screen: "S7-07  assigned to no one" in screen and "[a] Acknowledge" in screen,
                              "release text and receipt-only action")
        (evidence / "04-scope-release.screen.txt").write_text(screen)

        # Discover the root through A's real board, not a fabricated item id or
        # recipient-side lookup. Consent setup is arrival 1; assignment is 2.
        _command(sender, "/team board B")
        screen = _wait_screen(sender, lambda screen: bool(re.search(r"item\s+ri_[A-Za-z0-9_-]+\s*·\s*arrival\s+2", screen)),
                              "sender board supplies the assignment root")
        (evidence / "05-sender-board.screen.txt").write_text(screen)
        items = re.findall(r"item\s+(ri_[A-Za-z0-9_-]+)\s*·\s*arrival\s+(\d+)", screen)
        item = next(item for item, ordinal in items if ordinal == "2")
        assert turns[0]["answers"] == item, (turns, items)
        _command(sender, f"/team thread B {item}")
        screen = _wait_screen(sender, lambda screen: "[verdict: accept]" in screen,
                              "asker reads the typed verdict")
        (evidence / "06-sender-thread.screen.txt").write_text(screen)
        assert not _events(worlds["B"][0], "recipient_item_acknowledged"), "Accept is not acknowledgement"
        assert not any(row.get("turn") is not None or row.get("desync") for row in stub.rows()), stub.rows()
        for alias, (workspace, _) in worlds.items():
            rows = {path.name: path.read_text() for path in (workspace / ".rustain/rooms").glob("*.jsonl")}
            (evidence / (alias + ".journal.json")).write_text(json.dumps(rows, indent=2) + "\n")
        (evidence / "assertions.txt").write_text(
            "PASS: debug binary, 80x24, two real daemons and attached writers; standing pinned sender consent.\n"
            "PASS: setup card settled; story summary; native [y] Accept and [d] Discuss concerns; one durable human-decided Accept.\n"
            "PASS: scope_change release View S7-07  assigned to no one; [a] Acknowledge remains receipt only.\n"
            "PASS: assignment root discovered on sender board; /team thread shows [verdict: accept]; zero provider turns.\n")


@pytest.mark.a2a_binary
@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(300)
def test_scope_change_verdict_journey_80x24(monkeypatch):
    evidence_root = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())).resolve()
    evidence_root.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="19-22-", dir=evidence_root))
    manifest = {"run_id": evidence.name, "started_at": datetime.now(timezone.utc).isoformat(),
                "evidence_dir": str(evidence), "viewport": [80, 24], "result": "RUNNING"}
    identity = evidence / "run.json"
    try:
        binary = Path(os.environ["RUSTAIN_TUI_BINARY"]).resolve()
        assert binary.is_file() and "release" not in binary.parts, binary
        manifest.update(binary=str(binary), binary_sha256=_sha256(binary))
        identity.write_text(json.dumps(manifest, indent=2) + "\n")
        with tempfile.TemporaryDirectory(prefix="r19-22-", dir="/tmp") as root:
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
        print(f"NATIVE_19_22_EVIDENCE={evidence}", flush=True)
