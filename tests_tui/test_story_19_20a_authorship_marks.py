"""Native 80×24 recipient provenance and local outgoing-response retraction."""
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
import urllib.request
import urllib.error

import pytest
import harness
from decision_frame_helpers import open_decision
from fixtures.scene_provider import SceneStub
from harness import (
    RustainTUI,
    room_events,
    send_pty_command,
    sha256_file,
    stop_process_group,
    wait_until,
)

EVIDENCE_ROOT = Path(__file__).resolve().parents[2] / "_bmad-output/implementation-artifacts/19-20a-native-smoke"
AUTO = "Received. Reviewing."
APPROVED = "Consultation reviewed: preserve publication ordering."
DELEGATION = "Review the supplied Act 5 message."
BUG = "Session token not refreshed on OAuth→password switch"


def _stable_decision(tui, title, rail, timeout=60):
    """Wait for the complete painted frame, unchanged beyond its inert window."""
    deadline = time.monotonic() + timeout
    previous, since = None, None
    while time.monotonic() < deadline:
        screen = tui.get_screen_text()
        rows = screen.splitlines()
        frame = None
        for index, row in enumerate(rows):
            if title not in row or "┌" not in row or "┐" not in row:
                continue
            left = row.rfind("┌", 0, row.index(title))
            right = row.find("┐", row.index(title))
            for end in range(index + 1, len(rows)):
                if len(rows[end]) > right and rows[end][left] == "└" and rows[end][right] == "┘":
                    candidate = tuple(line[left:right + 1] for line in rows[index:end + 1])
                    if any((content := line[1:-1].strip().removeprefix("▶ ").lstrip()).startswith("[")
                           and rail in content for line in candidate[1:-1]):
                        frame = candidate
                    break
            break
        now = time.monotonic()
        if frame != previous:
            previous, since = frame, now
        elif frame is not None and now - since >= 0.6:
            return screen
    pytest.fail(f"Decision frame did not settle: {title}\n{screen}")


def _json(path, data):
    path.write_text(json.dumps(data, indent=2) + "\n")


def _provider_turns(stub, evidence):
    rows = stub.rows()
    turns = [(row["persona"], row["turn"], row["tool_results"])
             for row in rows if "turn" in row]
    desyncs = [row["n"] for row in rows if row.get("desync")]
    _json(evidence / "provider-turns.json", {"turns": turns, "desyncs": desyncs})
    assert not desyncs, desyncs
    return turns


def _world(root, alias, stub):
    ws = root / alias
    (ws / ".rustain").mkdir(parents=True)
    (ws / ".env").write_text("")
    home = root / (alias + "-home")
    home.mkdir()
    env = {**os.environ, **stub.env(alias, home), "TERM": "xterm-256color",
           "RUSTAIN_CONFIG_DIR": str(ws / ".rustain"),
           "RUSTAIN_DATA_DIR": str(ws / ".rustain_data"),
           "RUSTAIN_LOG_PATH": str(ws / "runtime.log")}
    (ws / ".rustain/a2a-interaction.toml").write_text(
        '[interaction.defaults]\nresponse_mode = "notify-and-auto"\nnotification = "immediate"\n')
    return ws, env


def _start_tui(stack, binary, ws, env, *, attached=False):
    session = stack.enter_context(RustainTUI(
        binary=binary, build=False, fresh=not attached, workspace=ws, env_overrides=env,
        extra_args=["daemon", "attach"] if attached else None,
    ))
    assert session.wait_for_screen("Ready", timeout=30), session.get_screen_text()
    return session


def _capture(evidence, name, screen):
    (evidence / (name + ".screen.txt")).write_text(screen)
    return screen


def _item_inspector(screen, address):
    """Return only the complete sheet for this durable item; no marks gate selection."""
    lines = screen.splitlines()
    if not lines or not lines[0].startswith("┌Thread · ") or not lines[0].endswith("┐"):
        return None
    end = next((i for i, line in enumerate(lines)
                if line.startswith("└") and line.endswith("┘")), None)
    if end is None or end < 3 or not all(line.startswith("│") and line.endswith("│")
                                         for line in lines[1:end]):
        return None
    rows = tuple(line[1:-1].strip() for line in lines[1:end])
    item = address["item"]
    identity = f"received thread · {address['principal']['peer']} · {item}"
    if (rows[0] == f"item actions · {item}"
            and identity in " ".join(rows)
            and "[Esc] Close" in " ".join(rows[-2:])):
        return rows
    return None


def _exercise_recipient_marks(binary, evidence, root):
    evidence.mkdir()
    tool = "a2a__recipient__native-review"
    usage = {"input_tokens": 10, "output_tokens": 10}
    turns = []
    for prompt, done in [("FIRST-DELEGATION", "FIRST-COMPLETE"),
                         ("SECOND-DELEGATION", "SECOND-COMPLETE")]:
        turns.extend([
            {"expect": prompt, "content": [{"type": "tool_use", "name": tool,
              "input": {"message": DELEGATION}}], "stop_reason": "tool_use", "usage": usage},
            {"expect": "Received", "content": [{"type": "text", "text": done}],
             "stop_reason": "end_turn", "usage": usage},
        ])
    scene = evidence / "scene.json"
    _json(scene, {"scene": "19-20-act5-native", "personas": {
        "sender": {"turns": turns}, "recipient": {"turns": []},
    }})
    with ExitStack() as stack:
        stub = stack.enter_context(SceneStub(scene, root / "requests.jsonl"))
        sender_ws, sender_env = _world(root, "sender", stub)
        received_ws, received_env = _world(root, "recipient", stub)
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            port = reservation.getsockname()[1]
            endpoint = f"http://127.0.0.1:{port}/"
            for ws, peers in ((sender_ws, {"recipient": {"url": endpoint}}),
                              (received_ws, {})):
                _json(ws / ".rustain/a2a.json", {
                    "server": {"admission": "allow"}, "agents": peers,
                })
            skill = received_ws / ".agents/skills/native-review/SKILL.md"
            skill.parent.mkdir(parents=True)
            skill.write_text(
                "---\nname: native-review\ndescription: Native delegation fixture\n"
                "---\nReview the supplied message.\n")
        log = stack.enter_context((received_ws / "daemon.log").open("w"))
        daemon = subprocess.Popen([
            str(binary), f"--serve-a2a=127.0.0.1:{port}", "daemon", "start", "--foreground",
        ], cwd=received_ws, env=received_env, stdout=log, stderr=subprocess.STDOUT,
            start_new_session=True)
        stack.callback(stop_process_group, daemon)

        def advertised():
            assert daemon.poll() is None, "recipient daemon exited"
            try:
                with urllib.request.urlopen(endpoint + ".well-known/agent-card.json", timeout=1) as response:
                    card = json.load(response)
                    return card if any(s["id"] == "native-review" for s in card["skills"]) else None
            except (OSError, urllib.error.URLError):
                return None

        card = wait_until(advertised, lambda: "native-review not advertised")
        assert card["x-rustain-ownership"]["kind"] == "self" and card["signatures"], card
        _json(evidence / "recipient-agent-card.json", card)
        recipient = _start_tui(stack, binary, received_ws, received_env, attached=True)
        sender = _start_tui(stack, binary, sender_ws, sender_env)

        def arrivals(count):
            rows = room_events(received_ws, "recipient_item_received")
            if len(rows) >= count:
                assert len(rows) == count, rows
                return rows
            return None

        def allow_once(name):
            if "[y] Allow once" in recipient.get_screen_text():
                consent = open_decision(recipient, "Peer consent", ("[y] Allow once",), timeout=60)
                _capture(evidence, name + "-sender-consent", consent)
                recipient.send("y")

        def finish(marker, *, session_granted=False):
            def completed():
                text = sender.get_screen_text()
                if session_granted:
                    assert "Decision: Permission" not in text and "[r] Review" not in text, text
                allow_once(marker)
                return text if marker in text else None

            text = wait_until(completed, sender.get_screen_text, timeout=60)
            _capture(evidence, marker, text)

        # Establish standing consent on the attached writer before delegation.
        send_pty_command(sender, "/team send recipient consent-setup")
        consent = open_decision(recipient, "Peer consent", ("[a] Always allow",), timeout=60)
        _capture(evidence, "sender-consent", consent)
        recipient.send("a")
        setup, = wait_until(lambda: arrivals(1), recipient.get_screen_text, timeout=60)
        assert setup["content"] == "consent-setup", setup

        send_pty_command(sender, "FIRST-DELEGATION")
        permission = open_decision(sender, "Permission", ("[r] Review", "[s] Session"), timeout=60)
        _capture(evidence, "operator-approval", permission)
        assert tool in permission and DELEGATION in permission, permission
        sender.send("s")
        finish("FIRST-COMPLETE")
        _, operator = wait_until(lambda: arrivals(2), recipient.get_screen_text)
        send_pty_command(sender, "SECOND-DELEGATION")
        finish("SECOND-COMPLETE", session_granted=True)
        _, first, policy = wait_until(lambda: arrivals(3), recipient.get_screen_text)
        assert first == operator
        assert "[r] Review" not in sender.get_screen_text(), "session grant must not prompt again"

        send_pty_command(sender, f"/team send --type bug_report recipient {BUG}")

        def bug_arrived():
            allow_once("bug-report")
            return arrivals(4)

        received = wait_until(bug_arrived, sender.get_screen_text, timeout=60)
        assert received[:3] == [setup, operator, policy], received
        bug = received[3]
        principal = setup["address"]["principal"]
        assert principal["kind"] == "a2a_pseudonym" and principal["peer"], principal
        assert len({row["address"]["item"] for row in received}) == 4, received
        assert len({row["task"] for row in received}) == 4, received
        assert all(row["address"]["principal"] == principal for row in received), received
        assert [(row.get("message_type"), row["content"], row["authorship"],
                 row["send_authorization"]) for row in received[1:]] == [
            (None, DELEGATION, "agent-composed", "human-decided"),
            (None, DELEGATION, "agent-composed", "automatic"),
            ("bug_report", BUG, "human-written", "human-decided"),
        ], received
        _json(evidence / "received-items.json", received[1:])

        for item, name, marks, actions in (
            (policy, "policy", (1, 1), ()),
            (operator, "operator", (1, 0), ()),
            (bug, "bug-report", (0, 0), ("[a] Acknowledge", "[d] Discuss")),
        ):
            if "item actions · " in recipient.get_screen_text():
                recipient.send("\x1b")
                wait_until(lambda: "item actions · " not in recipient.get_screen_text(),
                           recipient.get_screen_text, timeout=60)
            selected = item["address"]["item"]
            send_pty_command(recipient, f"/team view {selected}")

            def matching_frame():
                screen = recipient.get_screen_text()
                rows = _item_inspector(screen, item["address"])
                return (screen, rows) if rows else None

            screen, rows = wait_until(matching_frame, recipient.get_screen_text, timeout=60)
            _capture(evidence, name, screen)
            text = "\n".join(rows)
            rendered_item, = [row for row in rows if row.startswith(f"{selected} · received")]
            assert "[truncated]" not in text and item["content"] in rows, screen
            if item.get("message_type"):
                assert rows[1] == item["message_type"], screen
            assert all(action in text for action in actions), screen
            for mark, count in zip(("[agent-composed]", "[auto-sent]"), marks):
                assert rendered_item.count(mark) == text.count(mark) == count, screen
        assert arrivals(4) == received
    assert _provider_turns(stub, evidence) == [
        ("sender", 1, 0), ("sender", 2, 1), ("sender", 3, 0), ("sender", 4, 1),
    ]


def _room_entries(workspace, event):
    """Read actual journal sequences, not their display or nested event order."""
    records = []
    for path in sorted((workspace / ".rustain/rooms").glob("*.jsonl")):
        with path.open() as stream:
            for line in stream:
                if not line.endswith("\n"):
                    break
                row = json.loads(line)
                record = row["record"]
                if record["kind"] == "room" and record["payload"]["event"] == event:
                    records.append((path.name, row["seq"], record["payload"]))
    return records


@pytest.mark.a2a_binary
def test_authorship_marks_and_older_auto_retraction(monkeypatch):
    evidence_root = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", EVIDENCE_ROOT)).resolve()
    evidence_root.mkdir(parents=True, exist_ok=True)
    started = datetime.now(timezone.utc).isoformat()
    evidence = Path(tempfile.mkdtemp(prefix="19-20a-", dir=evidence_root))
    manifest = {"run_id": evidence.name, "started_at": started,
                "evidence_dir": str(evidence), "result": "RUNNING", "viewport": "80x24",
                "binary": os.environ.get("RUSTAIN_TUI_BINARY")}
    identity = evidence / "run.json"
    identity.write_text(json.dumps(manifest, indent=2) + "\n")
    try:
        binary = Path(os.environ["RUSTAIN_TUI_BINARY"]).resolve()
        manifest["binary"] = str(binary)
        assert binary.is_file(), binary
        assert "debug" in binary.parts and "release" not in binary.parts, binary
        manifest.update({"binary_sha256": sha256_file(binary),
                         "binary_bytes": binary.stat().st_size})
        identity.write_text(json.dumps(manifest, indent=2) + "\n")
        # Keep production AF_UNIX sockets below SUN_LEN, and remove private
        # workspaces/homes only after all daemon and PTY contexts have exited.
        monkeypatch.setattr(harness, "TERM_COLS", 80)
        monkeypatch.setattr(harness, "TERM_ROWS", 24)
        with tempfile.TemporaryDirectory(prefix="r20a-", dir="/tmp") as workspace:
            root = Path(workspace)
            recipient_root = root / "act5"
            recipient_root.mkdir()
            _exercise_recipient_marks(binary, evidence / "act5", recipient_root)
            _exercise_authorship_marks(binary, evidence, root / "outgoing")
        assert sha256_file(binary) == manifest["binary_sha256"], "native binary changed during capture"
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
            str(path.relative_to(evidence)): {"sha256": sha256_file(path), "bytes": path.stat().st_size}
            for path in sorted(evidence.rglob("*")) if path.is_file() and path != identity
        }
        identity.write_text(json.dumps(manifest, indent=2) + "\n")


def _exercise_authorship_marks(binary, evidence, tmp_path):
    tmp_path.mkdir()
    scene = evidence / "scene.json"
    scene.write_text(json.dumps({"scene": "19-20a-native", "personas": {
        "answerer": {"turns": [{"expect": "CONSULTATION-19-20A", "content": [
            {"type": "text", "text": APPROVED}], "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 10}}]},
        "sender": {"turns": []}}}))
    with ExitStack() as stack:
        stub = stack.enter_context(SceneStub(scene, tmp_path / "requests.jsonl"))
        worlds = {}
        reservations = {}
        for alias in ("sender", "answerer"):
            ws, env = _world(tmp_path, alias, stub)
            reservation = socket.socket()
            reservation.bind(("127.0.0.1", 0))
            reservations[alias] = reservation
            stack.callback(reservation.close)
            worlds[alias] = (ws, env, reservation.getsockname()[1])
        for alias, (ws, env, port) in worlds.items():
            peers = {peer: {"url": f"http://127.0.0.1:{world[2]}/"}
                     for peer, world in worlds.items() if peer != alias}
            (ws / ".rustain/a2a.json").write_text(json.dumps({"server": {"admission": "allow"}, "agents": peers}))
            if alias == "answerer":
                (ws / ".rustain/team-policy.toml").write_text(
                    '[team.overrides.consultation]\nresponse_mode = "notify-and-draft"\n')
            reservations[alias].close()
            log = stack.enter_context((ws / "daemon.log").open("w"))
            process = subprocess.Popen([str(binary), f"--serve-a2a=127.0.0.1:{port}",
                "daemon", "start", "--foreground"], cwd=ws, env=env,
                stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            stack.callback(stop_process_group, process)
            def ready():
                assert process.poll() is None, "daemon exited before agent card"
                try:
                    with urllib.request.urlopen(
                            f"http://127.0.0.1:{port}/.well-known/agent-card.json",
                            timeout=1) as response:
                        return response.status == 200
                except (OSError, urllib.error.URLError):
                    return False
            wait_until(ready, lambda: "Awaiting product agent card")
        answerer = _start_tui(stack, binary, *worlds["answerer"][:2], attached=True)
        sender = _start_tui(stack, binary, *worlds["sender"][:2])

        def save(name):
            return _capture(evidence, name, answerer.get_screen_text())

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
            for _ in range(32):
                text = answerer.get_screen_text()
                rows = text.splitlines()
                for i, row in enumerate(rows):
                    if body in row:
                        def boundary(line):
                            return not line.strip() or any(
                                role in line for role in ("Assistant:", "System:", "You:")
                            )

                        start = next((j for j in range(i - 1, -1, -1) if boundary(rows[j])), None)
                        if start is not None and "Assistant:" in rows[start]:
                            end = next((j for j in range(i + 1, len(rows)) if boundary(rows[j])), None)
                            if end is not None:
                                result = "\n".join(rows[start:end])
                                _capture(evidence, name, text)
                                (evidence / (name + ".span.txt")).write_text(result)
                                return result
                answerer.send("\x1b[6~")
            pytest.fail("Complete rendered response span not found: " + body)

        send_pty_command(sender, "/team send --type status_request answerer STATUS-19-20A")
        wait_until(lambda: any(label in answerer.get_screen_text()
                               for label in ("[y] Allow once", "[auto-sent]")),
                   lambda: save("status-arrival-failure"), timeout=60)
        if "[y] Allow once" in answerer.get_screen_text():
            save("sender-consent")
            _stable_decision(answerer, "Decision: Peer consent", "[y] Allow once")
            answerer.send("y")
        wait("[auto-sent]", "older-auto")
        auto_before = span(AUTO, "auto-before")
        for mark in ("[agent-composed]", "[auto-sent]", "[✗] Retract"):
            assert auto_before.count(mark) == 1, auto_before
        send_pty_command(sender, "/team send --type consultation answerer CONSULTATION-19-20A")
        wait_until(lambda: any(label in answerer.get_screen_text()
                               for label in ("[y] Allow once", "Decision: Recipient draft", "Needs you (")),
                   lambda: save("consultation-arrival-failure"), timeout=60)
        if "[y] Allow once" in answerer.get_screen_text():
            save("consultation-consent")
            _stable_decision(answerer, "Decision: Peer consent", "[y] Allow once")
            answerer.send("y")
        wait_until(lambda: "Decision: Recipient draft" in answerer.get_screen_text()
                   or "Needs you (" in answerer.get_screen_text(), answerer.get_screen_text, 60)
        if "Decision: Recipient draft" not in answerer.get_screen_text():
            answerer.send("\x18q")
            wait_until(lambda: "Needs you" in answerer.get_screen_text()
                       and "Recipient draft" in answerer.get_screen_text(), answerer.get_screen_text)
            answerer.send("\r")
        draft = _stable_decision(answerer, "Decision: Recipient draft", "[y] Approve")
        assert "Peer draft:" in draft and APPROVED in draft, draft
        _capture(evidence, "draft-frame", draft)
        answerer.send("y")
        wait_until(lambda: "[y] Approve" not in answerer.get_screen_text(), answerer.get_screen_text)
        # Approval closes the decision; the reader is no longer opened implicitly.
        save("approved-newer")
        approved_before = span(APPROVED, "approved-before")
        assert approved_before.count("[agent-composed]") == 1, approved_before
        assert "[auto-sent]" not in approved_before and "[✗] Retract" not in approved_before, approved_before
        # Typed sends are pull-only. Read their actual remote tasks to terminal
        # before retracting (Hazard 6); this is observation, not a fabricated
        # disclosure or a second send path.
        answerer_ws, _, answerer_port = worlds["answerer"]
        items = room_events(answerer_ws, "recipient_item_received")
        assert [(row.get("message_type"), row["content"]) for row in items] == [
            ("status_request", "STATUS-19-20A"),
            ("consultation", "CONSULTATION-19-20A"),
        ], items
        assert items[0]["task"] != items[1]["task"], items
        older_item, newer_item = (row["address"]["item"] for row in items)
        assert older_item != newer_item and older_item in auto_before, auto_before
        for index, (received, body) in enumerate(zip(items, (AUTO, APPROVED))):
            def terminal():
                request = urllib.request.Request(
                    f"http://127.0.0.1:{answerer_port}/",
                    data=json.dumps({"jsonrpc": "2.0", "id": index,
                        "method": "tasks/get", "params": {"id": received["task"]}}).encode(),
                    headers={"Content-Type": "application/json"})
                with urllib.request.urlopen(request, timeout=5) as response:
                    result = json.load(response)
                assert "error" not in result, result
                return result["result"] if result["result"]["status"]["state"] == "completed" else None

            task = wait_until(terminal, lambda: "Remote task did not reach product-minted completed state")
            item_id = received["address"]["item"]
            assert task["id"] == received["task"] and task["metadata"]["x-rustain-item-id"] == item_id, task
            assert task["status"]["message"]["parts"] == [{"kind": "text", "text": body}], task
            _json(evidence / f"task-{index}.json",
                  {"task": task["id"], "item": item_id, "state": task["status"]["state"], "body": body})

        disclosed = _room_entries(answerer_ws, "peer_disclosure")
        assert [(row["task"], row["authorship"], row["send_authorization"], row["disclosed_bytes"])
                for _, _, row in disclosed] == [
            (items[0]["task"], "agent-composed", "automatic", len(AUTO.encode())),
            (items[1]["task"], "agent-composed", "human-decided", len(APPROVED.encode())),
        ], disclosed
        (older_room, older_seq, _), (newer_room, newer_seq, _) = disclosed
        assert older_room == newer_room and 0 < older_seq < newer_seq, disclosed
        assert not _room_entries(answerer_ws, "auto_response_retracted")

        # Story 21.2 AC13: Ctrl+X is the attached leader, never retract. The
        # retract route is the palette entry over the real retractable row.
        answerer.send("\x10")
        wait("Command Palette", "palette-open")
        answerer.send("retract: Received")
        wait("retract: Received. Reviewing.", "palette-retract-entry")
        answerer.send("\r")
        preview = _stable_decision(answerer, "Decision: Retract", "[y] Retract")
        _capture(evidence, "retract-preview", preview)
        assert "Retract this auto-sent message?" in preview, preview
        assert AUTO in preview and APPROVED not in preview.split("Retract this auto-sent message?", 1)[1], preview
        for card in ("[y] Approve", "[y] Allow", "[r] Review", "sender consent", "[y] Send"):
            assert card not in preview, preview
        answerer.send("y")
        wait("Retracted. Marked in your log", "retracted")
        auto_after = span(AUTO, "auto-after")
        for mark in ("[agent-composed]", "[auto-sent]", "[retracted", "[✗] Retract (used)"):
            assert auto_after.count(mark) == 1, auto_after
        assert AUTO in auto_after and older_item in auto_after and newer_item not in auto_after, auto_after
        approved_after = span(APPROVED, "approved-after")
        assert approved_after == approved_before, (approved_before, approved_after)
        retractions = _room_entries(answerer_ws, "auto_response_retracted")
        (room, seq, retraction), = retractions
        assert room == older_room and seq > newer_seq and retraction["target_seq"] == older_seq, retractions
        assert _room_entries(answerer_ws, "peer_disclosure") == disclosed
        _json(evidence / "retraction-journal.json", {"disclosures": disclosed, "retractions": retractions})
    assert _provider_turns(stub, evidence) == [("answerer", 1, 0)]

    (evidence / "assertions.txt").write_text(
        "PASS: session grant carried the second delegation without a reprompt; recipient views match their exact "
        "durable item IDs, provenance and bug-report body/actions.\n"
        "PASS: only the older automatic disclosure was retracted; its body and used control remain visible, "
        "and the complete human-approved response span and its disclosure are unchanged.\n"
        "PASS: isolated provider ledgers have the exact intended turns and no desync.\n")
