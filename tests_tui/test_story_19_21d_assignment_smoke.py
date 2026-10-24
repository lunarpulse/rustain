"""Story 19.21d: debug-native story distribution and recipient summary at 80x24."""
import base64
from contextlib import ExitStack
from datetime import datetime, timezone
import json
import os
from pathlib import Path
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
    scene.write_text(json.dumps({"scene": "19-21d-native", "personas": {
        "A": {"turns": []}, "B": {"turns": []}}}) + "\n")
    # RFC8032's public test vector provides a real deterministic sender identity,
    # not an alias-derived pin. Its private test seed is not a production secret.
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
        # Loopback binds deliberately collapse callers to one pseudonym. A TLS
        # network bind exercises real peerKeys attribution to the sender alias.
        endpoints["B"] = f"https://localhost:{reservations['B'].getsockname()[1]}/"
        cert = root / "recipient-leaf.pem"
        tls_key = root / "recipient-leaf.key"
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
                   "NATIVE_A_TO_B_KEY": "story-19-21d-native-test-key"}
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
        command = "/team send --type story_assignment --stories S7-01,S7-04=B,S7-05=B B Sprint 7 stories"
        _command(sender, command)
        screen = _wait_screen(recipient, lambda screen: "story_assignment · A" in screen and "3 stories, 2 assigned to you" in screen,
                              "typed recipient arrival summary")
        (evidence / "01-arrival-card.screen.txt").write_text(screen)
        recipient.send("\t")
        screen = _wait_screen(recipient, lambda screen: "received thread" in screen and "S7-04  assigned to you" in screen,
                              "opened manifest block")
        (evidence / "02-opened-item.screen.txt").write_text(screen)
        listed = [row for row in _events(worlds["B"][0], "recipient_item_received") if row["content"] == "Sprint 7 stories"]
        assert len(listed) == 1 and listed[0]["stories"]["state"] == "Listed", listed
        assert [row["holder"]["kind"] for row in listed[0]["stories"]["stories"]] == ["NoHolder", "You", "You"]
        assert not any(row.get("turn") is not None or row.get("desync") for row in stub.rows()), stub.rows()
        for alias, (workspace, _) in worlds.items():
            rows = {path.name: path.read_text() for path in (workspace / ".rustain/rooms").glob("*.jsonl")}
            (evidence / (alias + ".journal.json")).write_text(json.dumps(rows, indent=2) + "\n")
        (evidence / "assertions.txt").write_text(
            "PASS: debug binary, 80x24, two real daemons and attached writers; standing pinned sender consent.\n"
            "PASS: consent setup replied and its oldest card gone before distribution.\n"
            "PASS: story_assignment · A; 3 stories, 2 assigned to you; Tab opens S7-04  assigned to you.\n"
            "PASS: real Listed recipient journal; zero provider turns.\n")


@pytest.mark.a2a_binary
@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(240)
def test_story_assignment_card_and_view_80x24(monkeypatch):
    evidence_root = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())).resolve()
    evidence_root.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="19-21d-", dir=evidence_root))
    manifest = {"run_id": evidence.name, "started_at": datetime.now(timezone.utc).isoformat(),
                "evidence_dir": str(evidence), "viewport": [80, 24], "result": "RUNNING"}
    identity = evidence / "run.json"
    try:
        binary = Path(os.environ["RUSTAIN_TUI_BINARY"]).resolve()
        assert binary.is_file() and "release" not in binary.parts, binary
        manifest.update(binary=str(binary), binary_sha256=_sha256(binary))
        identity.write_text(json.dumps(manifest, indent=2) + "\n")
        with tempfile.TemporaryDirectory(prefix="r19-21d-", dir="/tmp") as root:
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
        print(f"NATIVE_19_21D_EVIDENCE={evidence}", flush=True)
