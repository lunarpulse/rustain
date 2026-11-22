"""Debug-native team card journeys at 80x24, through two attached writers."""
import base64
from contextlib import ExitStack, contextmanager
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
from harness import (
    StructuralTUI, room_events, send_pty_command, sha256_file, stop_process_group, wait_for_frame,
)
from decision_frame_helpers import (
    defer_frame, has_row, open_recipient, recipient_text, rows, view_recipient, wait_frame_resending,
)


def _guard_wait_mode(screen):
    assert "Received.Reviewing." not in "".join(screen.split()), screen


@contextmanager
def _world(binary, evidence, root, *, mode="notify-and-wait", minimal=False, design_override=False):
    evidence.mkdir()
    root.mkdir()
    scene = evidence / "scene.json"
    scene.write_text(json.dumps({"scene": evidence.name, "personas": {
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
            policy = f'[interaction.defaults]\nresponse_mode = "{mode}"\nnotification = "immediate"\n'
            if minimal:
                policy += 'status_detail_minimum = "minimal"\n'
            (config / "a2a-interaction.toml").write_text(policy)
            if alias == "B" and design_override:
                (config / "team-policy.toml").write_text(
                    '[team.overrides.design_update]\nresponse_mode = "notify-and-wait"\n')
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
            stack.callback(stop_process_group, process)
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
            tui = StructuralTUI(binary=binary, build=False, fresh=False, workspace=ws,
                                env_overrides=env, extra_args=["daemon", "attach"],
                                frame_guard=None if design_override else _guard_wait_mode)
            tui.screens = stack.enter_context((evidence / (alias + ".screens.jsonl")).open("w"))
            terminal_log = stack.enter_context((evidence / (alias + ".terminal.txt")).open("w"))
            stack.callback(tui.stop)
            tui.start()
            tui.child.delaybeforesend = 0
            tui.child.logfile_read = terminal_log
            clients[alias] = tui
        try:
            yield clients["A"], clients["B"], worlds["B"][0]
            requests = stub.rows()
            assert not any(row.get("turn") is not None or row.get("desync") for row in requests), requests
            if minimal:
                assert all(row["method"] == "GET" and row["path"] == "/v1/models?limit=1000"
                           and row["kind"] == "discovery" for row in requests), requests
        finally:
            for alias, (workspace, _) in worlds.items():
                journal = {path.name: path.read_text()
                           for path in sorted((workspace / ".rustain/rooms").glob("*.jsonl"))}
                (evidence / (alias + ".journal.json")).write_text(json.dumps(journal, indent=2) + "\n")


def _save_screen(evidence, label, screen):
    (evidence / f"{label}.screen.txt").write_text(screen)


def _logical(screen):
    return " ".join(screen.replace("│", " ").split())


def _received(workspace, body):
    items = [row for row in room_events(workspace, "recipient_item_received") if row["content"] == body]
    assert len(items) == 1, items
    address = items[0]["address"]
    assert address["principal"]["kind"] == "a2a_bound" and address["boundPeer"], items
    return items[0]


def _replies(workspace, address):
    return [row for row in room_events(workspace, "recipient_item_replied")
            if row["answers"] == address["item"]]


def _acks(workspace, address):
    return [row for row in room_events(workspace, "recipient_item_acknowledged")
            if row["address"] == address]


def _checkpoint(workspace, address, evidence, label):
    acts = {"replies": _replies(workspace, address), "acks": _acks(workspace, address)}
    (evidence / f"{label}.json").write_text(json.dumps({"address": address, **acts}, indent=2) + "\n")
    return acts


def _assert_no_acts(acts):
    assert acts == {"replies": [], "acks": []}, acts


def _listed(item, expected):
    stories = item["stories"]
    assert stories["state"] == "Listed", item
    assert [(story["key"], story["holder"]["kind"]) for story in stories["stories"]] == expected, item


def _inspector(screen, address, *required):
    lines = screen.splitlines()
    if not lines or not lines[0].startswith("┌Thread · "):
        return False
    end = next((i for i, line in enumerate(lines) if line.startswith("└") and line.rstrip().endswith("┘")), None)
    if end is None:
        return False
    frame = "\n".join(lines[:end + 1])
    painted = rows(frame)
    item = address["item"]
    return (f"item actions · {item}" in painted and f"received thread · A · {item}" in painted
            and all(has_row(frame, text) for text in required))


def _close_view(recipient):
    recipient.send("\x1b")
    wait_for_frame(recipient, lambda screen: (
        not screen.startswith("┌Thread · ") and not has_row(screen, "item actions ·")
        and has_row(screen, "Needs you (")), "inspector closed and decision still deferred")


def _queue_settled(screen):
    return not has_row(screen, "Needs you (") and not has_row(screen, "Decision: Recipient draft")


def _consent(sender, recipient, command):
    send_pty_command(sender, command)
    wait_frame_resending(recipient, "Decision: Peer consent", ("[a] Always allow",), sender,
                         lambda: send_pty_command(sender, command))
    recipient.send("a")


def _defer_and_reply(recipient, workspace, address, evidence, label, body):
    before = _checkpoint(workspace, address, evidence, label + "-before-defer")
    _assert_no_acts(before)
    defer_frame(recipient)
    assert _checkpoint(workspace, address, evidence, label + "-after-defer") == before
    send_pty_command(recipient, f"/team reply {address['item']} {body}")
    wait_for_frame(recipient, lambda screen: (
        has_row(screen.lower(), "reply recorded") and _queue_settled(screen) and _replies(workspace, address)),
        f"{label} reply and queue settled")
    after = _checkpoint(workspace, address, evidence, label + "-replied")
    assert [row["content"] for row in after["replies"]] == [body] and not after["acks"], after


def _manual_consent(sender, recipient, workspace, evidence):
    _consent(sender, recipient, "/team send B consent-setup")
    _save_screen(evidence, "consent-setup", open_recipient(recipient))
    setup = _received(workspace, "consent-setup")
    _defer_and_reply(recipient, workspace, setup["address"], evidence, "consent-setup", "setup-complete")


def _list_and_verdict(sender, recipient, workspace, evidence):
    _manual_consent(sender, recipient, workspace, evidence)

    body = "S7-09: Fix payment timeout"
    send_pty_command(sender, "/team send --type story_assignment --stories S7-09=B B " + body)
    screen = open_recipient(recipient)
    assert has_row(screen, "story_assignment · A") and has_row(screen, "1 story, 1 assigned to you"), screen
    _save_screen(evidence, "assignment-card", screen)
    assignment = _received(workspace, body)
    _listed(assignment, [("S7-09", "You")])
    address = assignment["address"]
    before_view = _checkpoint(workspace, address, evidence, "verdict-before-view")
    _assert_no_acts(before_view)
    view_recipient(recipient)
    screen = wait_for_frame(recipient, lambda screen: _inspector(screen, address, "[y] Accept", "[d] Discuss concerns"),
                            "typed verdict keys on the original assignment")
    _save_screen(evidence, "verdict-actions", screen)
    assert _checkpoint(workspace, address, evidence, "verdict-before-accept") == before_view
    recipient.send("y")
    recipient.send("\x1b")
    screen = wait_for_frame(recipient, lambda screen: (
        has_row(screen, "Accepted.") and not has_row(screen, "[y] Accept")
        and _queue_settled(screen) and _replies(workspace, address)), "Accept settles the pending decision")
    _save_screen(evidence, "verdict-accepted", screen)
    accepted = _checkpoint(workspace, address, evidence, "verdict-after-accept")
    turns = accepted["replies"]
    assert len(turns) == 1 and not accepted["acks"], accepted
    verdict = turns[0]
    assert verdict["kind"] == {"type": "accept"} and verdict["content"] == "Accepted." and verdict.get("via_node"), verdict
    assert verdict["address"]["boundPeer"] == address["boundPeer"], verdict
    assert verdict["authorship"] == "human-written" and verdict["send_authorization"] == "human-decided", verdict
    assert not room_events(workspace, "recipient_item_acknowledged"), "Accept is not acknowledgement"
    assert [row for row in room_events(workspace, "recipient_item_replied")
            if row.get("kind") == {"type": "accept"}] == turns

    body = "Sprint 7 scope change"
    send_pty_command(sender, "/team send --type scope_change --stories S7-09=B,S7-07 B " + body)
    screen = open_recipient(recipient)
    assert has_row(screen, "scope_change · A") and has_row(screen, "2 stories, 1 assigned to you"), screen
    _save_screen(evidence, "scope-card", screen)
    scope = _received(workspace, body)
    _listed(scope, [("S7-09", "You"), ("S7-07", "NoHolder")])
    scope_address = scope["address"]
    before_scope = _checkpoint(workspace, scope_address, evidence, "scope-before-view")
    _assert_no_acts(before_scope)
    view_recipient(recipient)
    screen = wait_for_frame(recipient, lambda screen: (
        _inspector(screen, scope_address, "S7-07  assigned to no one", "[a] Acknowledge")),
        "exact scope release and its receipt rail")
    _save_screen(evidence, "scope-release", screen)

    # Discover the assignment root on A. Consent setup is arrival 1; the
    # verdict must answer arrival 2, not the waiting scope release.
    send_pty_command(sender, "/team board B")
    arrivals = r"item\s+(ri_[A-Za-z0-9_-]+)\s*·\s*arrival\s+(\d+)"
    screen = wait_for_frame(sender, lambda screen: any(
        ordinal == "2" for _, ordinal in re.findall(arrivals, screen)), "sender board supplies the assignment root")
    _save_screen(evidence, "sender-board", screen)
    roots = [item for item, ordinal in re.findall(arrivals, screen) if ordinal == "2"]
    assert roots == [verdict["answers"]] == [address["item"]], (roots, verdict)
    send_pty_command(sender, f"/team thread B {roots[0]}")
    screen = wait_for_frame(sender, lambda screen: "[verdict: accept]" in screen, "asker reads the typed verdict")
    _save_screen(evidence, "sender-thread", screen)
    # The sent-thread inspector consumes paste and Enter; close it on A before
    # dispatching the manifest. B's scope decision remains pending.
    sender.send("\x1b")
    screen = wait_for_frame(sender, lambda screen: not screen.startswith("┌Thread · sent · "),
                            "sent thread closed before manifest dispatch")
    _save_screen(evidence, "sender-thread-closed", screen)
    _close_view(recipient)
    assert _checkpoint(workspace, scope_address, evidence, "scope-after-view") == before_scope

    # View deferred the scope card without deciding it. The next arrival gets
    # its own card, and its durable ID, never the current queue cursor, reopens it.
    body = "Sprint 7 stories"
    send_pty_command(sender, "/team send --type story_assignment --stories S7-01,S7-04=B,S7-05=B B " + body)
    # Do not open the old, deferred scope card while manifest dispatch is in flight.
    wait_for_frame(recipient, lambda screen: has_row(screen, "Needs you (2)"),
                   "manifest joins the already-deferred scope decision")
    screen = open_recipient(recipient)
    assert has_row(screen, "story_assignment · A") and has_row(screen, "3 stories, 2 assigned to you"), screen
    _save_screen(evidence, "manifest-arrival", screen)
    manifest = _received(workspace, body)
    _listed(manifest, [("S7-01", "NoHolder"), ("S7-04", "You"), ("S7-05", "You")])
    manifest_address = manifest["address"]
    before_manifest = _checkpoint(workspace, manifest_address, evidence, "manifest-before-view")
    _assert_no_acts(before_manifest)
    view_recipient(recipient)
    screen = wait_for_frame(recipient, lambda screen: _inspector(screen, manifest_address, "S7-04  assigned to you"),
                            "manifest View shows the assigned row")
    _save_screen(evidence, "manifest-view", screen)
    _close_view(recipient)
    send_pty_command(recipient, f"/team view {manifest_address['item']}")
    screen = wait_for_frame(recipient, lambda screen: _inspector(screen, manifest_address, "S7-04  assigned to you"),
                            "deferred manifest reopened by its exact ID")
    _save_screen(evidence, "manifest-reopened", screen)
    assert _checkpoint(workspace, manifest_address, evidence, "manifest-after-reopen") == before_manifest
    assert _checkpoint(workspace, scope_address, evidence, "scope-after-manifest") == before_scope
    assert _checkpoint(workspace, address, evidence, "verdict-final") == accepted


def _auto_settled(recipient, item, evidence, label):
    result = f"Received. Reviewing. Open: /team view {item['address']['item']}"
    screen = wait_for_frame(recipient, lambda screen: (
        _queue_settled(screen) and has_row(screen, result)), f"{label} auto-settled at the exact item")
    _save_screen(evidence, label, screen)


def _design_update(sender, recipient, workspace, evidence):
    _consent(sender, recipient, "/team send B consent-setup")
    wait_for_frame(recipient, lambda screen: (
        "consent-setup" in screen and "Always allow" not in screen and _queue_settled(screen)),
        "auto consent setup accepted with no pending draft")
    _auto_settled(recipient, _received(workspace, "consent-setup"), evidence, "design-consent-setup")

    body = "Sprint 7 stories"
    send_pty_command(sender, "/team send --type story_assignment --stories S7-06=B B " + body)
    wait_for_frame(recipient, lambda screen: (
        body in screen and any(row["content"] == body for row in room_events(workspace, "recipient_item_received"))),
        "holder assignment admitted")
    assignment = _received(workspace, body)
    _listed(assignment, [("S7-06", "You")])
    _auto_settled(recipient, assignment, evidence, "design-holder-assignment")

    body = "Updated payment flow: 2-step to 3-step"
    selected = "A selected you for story S7-06"
    send_pty_command(sender, "/team send --type design_update --holders-of S7-06 " + body)
    screen = open_recipient(recipient)
    assert has_row(screen, "design_update · A") and has_row(screen, selected), screen
    _save_screen(evidence, "design-arrival", screen)
    design = _received(workspace, body)
    assert design["selected_for"] == {"state": "Listed", "stories": ["S7-06"]}, design
    address = design["address"]
    before = _checkpoint(workspace, address, evidence, "design-before-view")
    _assert_no_acts(before)
    view_recipient(recipient)
    screen = wait_for_frame(recipient, lambda screen: _inspector(screen, address, selected, "[a] Acknowledge"),
                            "design selection View and receipt action")
    _save_screen(evidence, "design-view", screen)
    assert _checkpoint(workspace, address, evidence, "design-before-ack") == before
    assert not room_events(workspace, "recipient_item_acknowledged"), "View must not acknowledge"
    recipient.send("a")
    screen = wait_for_frame(recipient, lambda screen: (
        _inspector(screen, address) and not has_row(screen, "[a] Acknowledge") and _acks(workspace, address)),
        "design receipt action cleared after a durable acknowledgement")
    _save_screen(evidence, "design-acknowledged", screen)
    after = _checkpoint(workspace, address, evidence, "design-after-ack")
    acknowledgements = room_events(workspace, "recipient_item_acknowledged")
    assert len(after["acks"]) == 1 and after["acks"] == acknowledgements, acknowledgements
    assert after["replies"] == before["replies"] and not room_events(workspace, "recipient_item_replied"), after


def _minimal_status(sender, recipient, workspace, evidence):
    _manual_consent(sender, recipient, workspace, evidence)

    send_pty_command(sender, "/team send --type story_assignment --stories S7-03=B,S7-05=B B Sprint 7")
    screen = open_recipient(recipient)
    assert has_row(screen, "story_assignment · A") and has_row(screen, "2 stories, 2 assigned to you"), screen
    _save_screen(evidence, "status-assignment", screen)
    assignment = _received(workspace, "Sprint 7")
    _listed(assignment, [("S7-03", "You"), ("S7-05", "You")])
    _defer_and_reply(recipient, workspace, assignment["address"], evidence, "status-assignment", "assignment-received")

    body = "Give me a sprint progress report"
    send_pty_command(sender, "/team send --type status_request B " + body)
    screen = open_recipient(recipient)
    assert has_row(screen, "status_request · A"), screen
    assert "Shares (minimal): S7-03 · S7-05" in recipient_text(screen), screen
    assert "Not shared: conversations, plans, files, activity" in recipient_text(screen), screen
    _save_screen(evidence, "status-arrival", screen)
    address = _received(workspace, body)["address"]
    before = _checkpoint(workspace, address, evidence, "status-before-view")
    _assert_no_acts(before)
    view_recipient(recipient)
    screen = wait_for_frame(recipient, lambda screen: _inspector(screen, address, "[s] Share status"),
                            "status item actions")
    _save_screen(evidence, "status-view", screen)
    assert _checkpoint(workspace, address, evidence, "status-before-share") == before
    recipient.send("s")
    screen = wait_for_frame(recipient, lambda screen: (
        not has_row(screen, "[s] Share status") and any(
            row.get("status") is not None for row in _replies(workspace, address))),
        "exact request's durable status share clears the rail")
    _save_screen(evidence, "status-shared", screen)
    after = _checkpoint(workspace, address, evidence, "status-after-share")
    shared = [row for row in room_events(workspace, "recipient_item_replied") if row.get("status") is not None]
    assert len(shared) == 1 and shared == after["replies"] and not after["acks"], after
    assert shared[0]["status"] == {"level": "minimal", "stories": [{"key": "S7-03"}, {"key": "S7-05"}]}, shared

    send_pty_command(sender, "/team board --last")
    source = "Source: manually shared (B's policy: minimal) [agent-composed]"
    screen = wait_for_frame(sender, lambda screen: (
        "S7-03 · S7-05" in _logical(screen) and "(limited detail — B shares minimal)" in _logical(screen)
        and "".join(source.split()) in "".join(_logical(screen).split())),
        "policy-attributed agent-composed status on the sender board")
    _save_screen(evidence, "status-sender-board", screen)


def _consultation_and_retro(sender, recipient, workspace, evidence):
    for event in ("recipient_item_received", "recipient_item_replied", "recipient_item_acknowledged"):
        assert not room_events(workspace, event), event
    question = "Could you review the handoff?"
    _consent(sender, recipient, "/team send --type consultation B " + question)
    screen = open_recipient(recipient)
    assert has_row(screen, "consultation · A"), screen
    _save_screen(evidence, "consultation-arrival", screen)
    consultation = _received(workspace, question)["address"]
    before = _checkpoint(workspace, consultation, evidence, "consultation-before-view")
    _assert_no_acts(before)
    view_recipient(recipient)
    screen = wait_for_frame(recipient, lambda screen: _inspector(screen, consultation, "[r] Respond"),
                            "consultation response slot")
    _save_screen(evidence, "consultation-view", screen)
    assert _checkpoint(workspace, consultation, evidence, "consultation-before-respond") == before
    recipient.send("r")
    screen = wait_for_frame(recipient, lambda screen: f"/team reply {consultation['item']}" in _logical(screen),
                            "Respond prefills the exact consultation ID")
    _save_screen(evidence, "consultation-composer", screen)
    assert _checkpoint(workspace, consultation, evidence, "consultation-before-submit") == before
    recipient.send("Handoff reviewed.\r")
    wait_for_frame(recipient, lambda screen: _queue_settled(screen) and _replies(workspace, consultation),
                   "human consultation reply settled before requesting retro")
    replied = _checkpoint(workspace, consultation, evidence, "consultation-replied")
    assert [row["content"] for row in replied["replies"]] == ["Handoff reviewed."] and not replied["acks"], replied
    assert room_events(workspace, "recipient_item_replied") == replied["replies"], replied

    body = "Sprint 7 retro"
    send_pty_command(sender, "/team send --type retro_request B " + body)
    screen = open_recipient(recipient)
    assert has_row(screen, "retro_request · A"), screen
    assert "[1] received items: consultation 1" in recipient_text(screen), screen
    assert "[2] reply times: 1 reply, median <1m" in recipient_text(screen), screen
    _save_screen(evidence, "retro-arrival", screen)
    screen = wait_for_frame(sender, lambda screen: "Send · retro_request · ● B sent" in _logical(screen),
                            "the sender's retrospective outcome reads sent")
    _save_screen(evidence, "retro-sent", screen)
    address = _received(workspace, body)["address"]
    before = _checkpoint(workspace, address, evidence, "retro-before-view")
    _assert_no_acts(before)
    view_recipient(recipient)
    screen = wait_for_frame(recipient, lambda screen: _inspector(screen, address, "[s] Share", "[1-3] Leave out"),
                            "retrospective share and leave-out keys")
    _save_screen(evidence, "retro-view", screen)
    assert _checkpoint(workspace, address, evidence, "retro-before-toggle") == before
    recipient.send("2")
    screen = wait_for_frame(recipient, lambda screen: (
        _inspector(screen, address, "[s] Share with redactions")
        and "[2] reply times: left out" in _logical(screen) and "Not shared: reply times ·" in _logical(screen)),
        "reply-time exclusion preview and redacted share rail")
    _save_screen(evidence, "retro-redacted", screen)
    assert _checkpoint(workspace, address, evidence, "retro-after-toggle") == before
    recipient.send("s")
    screen = wait_for_frame(recipient, lambda screen: (
        not has_row(screen, "[s] Share") and any(row.get("kind") == {"type": "retro"}
                                               for row in _replies(workspace, address))),
        "retro reply persisted and share rail cleared")
    _save_screen(evidence, "retro-shared", screen)
    after = _checkpoint(workspace, address, evidence, "retro-after-share")
    shared = [row for row in room_events(workspace, "recipient_item_replied") if row.get("kind") == {"type": "retro"}]
    assert len(shared) == 1 and shared == after["replies"] and not after["acks"], after
    reply = shared[0]
    assert reply["retro"] == {"received": {"consultation": 1}} and reply["content"] == "Retrospective reply.", reply
    assert reply["authorship"] == "agent-composed" and reply["send_authorization"] == "human-decided", reply

    # Remove earlier sends from the canvas before reading the durable aggregate.
    send_pty_command(sender, "/clear")
    send_pty_command(sender, "/team board --last")
    honesty = "This report contains only what was consented. It does not report who contributed, or how many."
    screen = wait_for_frame(sender, lambda screen: (
        "Retrospective report · retro_request" in _logical(screen)
        and "Received items (14 days): consultation 1" in _logical(screen) and honesty in _logical(screen)),
        "aggregate-only retrospective report")
    report = screen[screen.index("Retrospective report"):]
    assert "Time to first reply" not in report and not re.search(r"\bB\b", report), report
    assert "addressed" not in report and "declined" not in report, report
    _save_screen(evidence, "retro-sender-report", screen)


def _exercise(binary, evidence, root, monkeypatch):
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    scenes = (
        ("assignment-and-scope", _list_and_verdict, {}),
        ("design", _design_update, {"mode": "notify-and-auto", "design_override": True}),
        ("status", _minimal_status, {"minimal": True}),
        ("retro", _consultation_and_retro, {"minimal": True}),
    )
    # Status folds all prior held stories; retro folds all prior replies. Their
    # ledgers must be fresh, and design needs the distinct auto response policy.
    for name, journey, policy in scenes:
        capture = evidence / name
        with _world(binary, capture, root / name, **policy) as (sender, recipient, workspace):
            journey(sender, recipient, workspace, capture)


@pytest.mark.a2a_binary
@pytest.mark.timeout(420)
def test_team_card_journey_80x24(monkeypatch):
    evidence_root = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())).resolve()
    evidence_root.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="19-22-", dir=evidence_root))
    manifest = {"run_id": evidence.name, "started_at": datetime.now(timezone.utc).isoformat(),
                "evidence_dir": str(evidence), "viewport": [80, 24], "result": "RUNNING"}
    identity = evidence / "run.json"
    try:
        binary = Path(os.environ["RUSTAIN_TUI_BINARY"]).resolve()
        assert binary.is_file() and "release" not in binary.parts, binary
        manifest.update(binary=str(binary), binary_sha256=sha256_file(binary))
        identity.write_text(json.dumps(manifest, indent=2) + "\n")
        with tempfile.TemporaryDirectory(prefix="r19-22-", dir="/tmp") as root:
            _exercise(binary, evidence, Path(root), monkeypatch)
        assert sha256_file(binary) == manifest["binary_sha256"], "binary changed during native smoke"
        manifest["result"] = "PASS"
    except BaseException:
        manifest["result"] = "FAIL"
        (evidence / "failure.txt").write_text(traceback.format_exc())
        raise
    finally:
        manifest["completed_at"] = datetime.now(timezone.utc).isoformat()
        identity.write_text(json.dumps(manifest, indent=2) + "\n")
        print(f"NATIVE_19_22_EVIDENCE={evidence}", flush=True)
