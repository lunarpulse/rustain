"""Story 19.19: operator-dispatched Act 2 through real PTYs and transports.

The scene is a deterministic provider, not the witness. Witnesses are painted
product controls, real tool/file results, complete request observations and the
product journals/team log. Loopback shares a principal; this is not identity proof
or the agent-mediated 19.26 journey. No direct item/core/socket writes occur here.
"""
from __future__ import annotations

from contextlib import ExitStack
from datetime import datetime, timezone
import hashlib
import itertools
import json
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
import uuid

import pytest

import harness
from harness import PROJECT_ROOT, RustainTUI
from fixtures.scene_provider import SceneStub

pytestmark = [pytest.mark.story_19_19, pytest.mark.a2a_binary]
SCENE = Path(__file__).parent / "journeys/j10/scenes/19-19-act2-v1.json"
ADR = Path(__file__).parent / "journeys/j10/fixtures/ADR-047.md"
QUESTION = (
    "CONSULT-BYTES-19-19\n"
    "snippet before:\nfn publish() { show_card(); persist_row(); }\n"
    "snippet after:\nfn publish() { persist_row(); show_card(); }\n"
    "error[E0609]: no field `state` on type `Card`\n"
    "Keep pending   queue ownership; wide: 界界."
)
ANSWER = "Persist before publishing; retain pending queue ownership.\n  Keep the two operations ordered."
ADR_EXPECTED = (
    "# ADR-047: Peer consultation delivery\n\nStatus: accepted\n\n"
    "Decision: Persist received peer rows before publishing live cards.\n\n"
    "Rationale: Pending consultation responses retain their own queue ownership.\n\n"
    "Evidence marker: ADR047-LIVE-PUBLISH-AFTER-PERSIST\n"
)
DEFER_HEADER = "[defer 30m — reported expectation]"


def _sha(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def _wait(predicate, diagnostic, timeout=30):
    deadline = time.monotonic() + timeout
    while True:
        value = predicate()
        if value:
            return value
        assert time.monotonic() < deadline, diagnostic()
        time.sleep(0.04)


def _stop(process):
    if process.poll() is None:
        os.killpg(process.pid, signal.SIGTERM)
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
    process.wait()


def _defer_header_visible(text):
    # Cell wrapping adds whitespace and a viewport border, not new content.
    compact = lambda value: re.sub(r"[\s│]", "", value)
    return compact(DEFER_HEADER) in compact(text)


def _events(workspace, name):
    found = []
    def visit(value):
        if isinstance(value, dict):
            if value.get("event") == name:
                found.append(value)
            else:
                for child in value.values():
                    visit(child)
        elif isinstance(value, list):
            for child in value:
                visit(child)
    for path in sorted((workspace / ".rustain/rooms").glob("*.jsonl")):
        for line in path.read_text().splitlines():
            visit(json.loads(line))
    return found


def _command(tui, text):
    # Exactly one Enter: embedded newlines are bracketed paste, not submissions.
    tui.send("\x1b[200~" + text + "\x1b[201~")
    tui.send("\r")


def _turns(stub, alias):
    return [row for row in stub.rows() if row.get("persona") == "scene-" + alias and "turn" in row]


def _ledger(workspace):
    return [json.loads(line) for path in sorted((workspace / ".rustain_data/usage").glob("*.jsonl"))
            for line in path.read_text().splitlines() if line.strip()]


@pytest.mark.parametrize("columns", [80, 130])
@pytest.mark.timeout(420)
def test_act2_consultation_adr_fanout(columns, monkeypatch):
    binary_name = os.environ.get("RUSTAIN_TUI_BINARY")
    assert binary_name, "Act 2 requires an explicitly built A2A binary, not a fallback or skip"
    binary = Path(binary_name).resolve()
    assert binary.is_file(), binary
    root = Path(tempfile.mkdtemp(dir="/tmp", prefix=f"r19-act2-{columns}-"))
    identity = f"{columns}-{datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%S%fZ')}-{uuid.uuid4().hex}"
    evidence_root = Path(os.environ.get(
        "RUSTAIN_CAPTURE_EVIDENCE_DIR",
        Path(__file__).parent / "journeys/j10/receipts/evidence",
    )) / identity
    evidence_root.mkdir(parents=True, exist_ok=False)
    # Production socket: <data-dir>/daemons/<16-character hash>.sock; AF_UNIX limit is ~108.
    assert len(str(root / "marcus" / ".rustain_data" / "daemons" / ("a" * 16 + ".sock"))) <= 100
    binary_sha = _sha(binary)
    monkeypatch.setenv("RUSTAIN_TUI_BINARY", str(binary))
    monkeypatch.setattr(harness, "TERM_COLS", columns)
    monkeypatch.setattr(harness, "TERM_ROWS", 24 if columns == 80 else 30)
    stamp_time = datetime.now(timezone.utc).replace(microsecond=0)
    stamp = stamp_time.strftime("%Y-%m-%dT%H-%M-%SZ")
    recorded = stamp_time.strftime("%Y-%m-%dT%H:%M:%SZ")
    snapshots = []
    resources = ExitStack()
    def retain_evidence():
        # Only product evidence named here is exported; homes, configs and credentials stay private.
        for path in root.rglob("*"):
            if not path.is_file():
                continue
            name = path.name
            if (name == "requests.jsonl" or name.endswith(".screen.txt")
                    or name.endswith(".daemon-0.log") or name.endswith(".daemon-1.log")
                    or name.endswith(".runtime.log") or name.endswith(".team-log.json")
                    or name.endswith(".terminal.txt") or name.endswith(".jsonl")
                    and ".rustain/rooms" in path.as_posix()):
                relative = path.relative_to(root)
                target = evidence_root / relative
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(path, target)
        receipt_dir = Path(__file__).parent / "journeys/j10/receipts" / identity
        if receipt_dir.is_dir():
            shutil.copytree(receipt_dir, evidence_root / "receipt", dirs_exist_ok=True)
    resources.callback(retain_evidence)
    with resources:
        stub = resources.enter_context(SceneStub(SCENE, root / "requests.jsonl"))
        workspaces = {alias: root / alias for alias in ("jun", "marcus", "dev2")}
        homes = {alias: root / (alias + "-home") for alias in workspaces}
        reservations = {}
        ports = {}
        for alias in workspaces:
            workspaces[alias].mkdir()
            homes[alias].mkdir()
            reservation = socket.socket()
            reservation.bind(("127.0.0.1", 0))
            ports[alias] = reservation.getsockname()[1]
            reservations[alias] = reservation
        endpoints = {alias: f"http://127.0.0.1:{port}/" for alias, port in ports.items()}
        environments = {}
        daemons = {}
        for alias, workspace in workspaces.items():
            config = workspace / ".rustain"
            config.mkdir()
            (config / "a2a.json").write_text(json.dumps({
                "server": {"admission": "allow"},
                "agents": {peer: {"url": endpoint} for peer, endpoint in endpoints.items() if peer != alias},
            }))
            (config / "a2a-interaction.toml").write_text(
                '[interaction.defaults]\nresponse_mode = "notify-and-wait"\nnotification = "immediate"\n')
            (config / "config.toml").write_text('[permissions]\nalways_tools = ["Read"]\n')
            environments[alias] = {
                **os.environ, **stub.env("scene-" + alias, homes[alias]),
                "TERM": "xterm-256color", "RUSTAIN_CONFIG_DIR": str(config),
                "RUSTAIN_DATA_DIR": str(workspace / ".rustain_data"),
                "RUSTAIN_LOG_PATH": str(root / (alias + ".runtime.log")),
            }
        # Seeding the OLD fixture is the only driver write to the ADR. The
        # required changed bytes below must be written by the actual Write tool.
        (workspaces["marcus"] / "ADR-047.md").write_bytes(ADR.read_bytes())
        canary = workspaces["jun"] / "peer-canary.txt"
        canary.write_text("unchanged native canary\n")

        def start_daemon(alias, generation=0):
            log_path = root / f"{alias}.daemon-{generation}.log"
            log = resources.enter_context(log_path.open("w"))
            reservation = reservations.pop(alias, None)
            if reservation is not None:
                reservation.close()
            process = subprocess.Popen(
                [str(binary), f"--serve-a2a=127.0.0.1:{ports[alias]}", "daemon", "start", "--foreground"],
                cwd=workspaces[alias], env=environments[alias], stdout=log,
                stderr=subprocess.STDOUT, start_new_session=True)
            resources.callback(_stop, process)
            daemons[alias] = process
            def ready():
                assert process.poll() is None, log_path.read_text()
                try:
                    with urllib.request.urlopen(endpoints[alias] + ".well-known/agent-card.json", timeout=1) as reply:
                        return reply.status == 200
                except (OSError, urllib.error.URLError):
                    return False
            _wait(ready, log_path.read_text)

        for alias in workspaces:
            start_daemon(alias)

        def release_reservations():
            for reservation in reservations.values():
                reservation.close()
            reservations.clear()
        resources.callback(release_reservations)

        sessions = itertools.count()

        def start_tui(alias, attached=False, fresh=False, read_only=False):
            tui = resources.enter_context(RustainTUI(
                fresh=fresh, build=False, workspace=workspaces[alias], allowed_tools=["Read"],
                extra_args=["daemon", "attach"] if attached else ["-c", "layout.density_mode=monitor"],
                env_overrides=stub.env("scene-" + alias, homes[alias]), timeout=30))
            raw = root / f"{alias}-{'attached' if attached else 'standalone'}-{next(sessions)}.terminal.txt"
            tui.child.logfile_read = resources.enter_context(raw.open("w"))
            assert tui.wait_for_screen("Read-only." if read_only else "Ready", timeout=30), tui.get_screen_text()
            return tui

        def save(tui, name):
            text = tui.get_screen_text()
            path = root / (name + ".screen.txt")
            path.write_text(text)
            snapshots.append(path.name)
            return text

        def pages(tui, name, limit=12, recipient=True):
            seen = []
            if recipient:
                for _ in range(limit):
                    tui.send("\x1b[5~")
                time.sleep(0.1)
                for index in range(limit):
                    text = save(tui, f"{name}-{index}")
                    seen.append(text)
                    tui.send("\x1b[6~")
                    time.sleep(0.08)
                return "\n".join(seen)

            # Ordinary feedback blocks scroll only with Chat focus controls.
            tui.send("\x1b")  # Input -> Chat; Esc is a focus toggle in this state.
            for _ in range(limit):
                tui.send("\x02")  # Ctrl+B: full page up
                time.sleep(0.03)
            previous = None
            exhausted = False
            for index in range(limit * 2):
                text = save(tui, f"{name}-{index}")
                if text == previous:
                    exhausted = True
                    break
                seen.append(text)
                previous = text
                tui.send("\x04")  # Ctrl+D: scroll half page down
                time.sleep(0.08)
            assert exhausted, f"{name}: chat scrolling never reached its end"
            tui.send("\x1b")  # restore Input focus for the next command
            return "\n".join(seen)

        def open_view(tui, item, name):
            _command(tui, "/team view " + item)
            assert tui.wait_for_screen("item actions ·", timeout=10), tui.get_screen_text()
            painted = save(tui, name)
            assert item in painted, painted
            return painted

        def painted_item(tui, alias):
            tui.send("\t")
            assert tui.wait_for_screen("item actions ·", timeout=10), tui.get_screen_text()
            painted = save(tui, alias + "-canonical-item")
            match = re.search(r"item actions · (ri_[A-Za-z0-9_-]+)", painted)
            assert match, painted
            return match.group(1)

        def close_view(tui):
            tui.send("\x1b")
            _wait(lambda: "item actions ·" not in tui.get_screen_text(), tui.get_screen_text)

        def thread(tui, peer, item, name):
            # A completed read can be near-instant, so prove freshness by first
            # removing the previous rendered thread, then requiring it to reappear.
            _command(tui, "/clear")
            _wait(lambda: "thread · " + peer not in tui.get_screen_text(), tui.get_screen_text)
            _command(tui, f"/team thread {peer} {item}")
            assert tui.wait_for_screen("loopback bind", timeout=30), tui.get_screen_text()
            text = pages(tui, name, recipient=False)
            assert "thread · " + peer in text and item in text, text
            return text

        writers = {alias: start_tui(alias, attached=True) for alias in workspaces}
        marcus_reader = start_tui("marcus", attached=True, read_only=True)
        for alias, tui in writers.items():
            save(tui, alias + "-attached-before-arrival")
        jun_source = start_tui("jun", fresh=True)
        _command(jun_source, "/team send --type consultation marcus " + QUESTION)
        marcus = writers["marcus"]
        assert marcus.wait_for_screen("[y] Allow once", timeout=30), marcus.get_screen_text()
        marcus.send("y")
        for tui in (marcus, marcus_reader):
            assert tui.wait_for_screen("consultation ·", timeout=30), tui.get_screen_text()
        consultation = painted_item(marcus, "marcus-consultation")
        question_pages = pages(marcus, "marcus-question")
        assert f"1 text body · {len(QUESTION.encode())} bytes" in question_pages, question_pages
        for line in ("fn publish() { show_card(); persist_row(); }", "fn publish() { persist_row(); show_card(); }",
                     "error[E0609]: no field `state` on type `Card`"):
            assert line in question_pages, question_pages
        received = _events(workspaces["marcus"], "recipient_item_received")
        assert len(received) == 1 and received[0]["address"]["item"] == consultation, received
        assert received[0]["content"] == QUESTION, received
        assert not _turns(stub, "marcus") and not _turns(stub, "jun"), stub.rows()
        marcus.send("b")
        assert marcus.wait_for_screen("reply recorded for", timeout=30), marcus.get_screen_text()
        save(marcus, "marcus-busy-recorded")
        defers = _events(workspaces["marcus"], "recipient_item_replied")
        assert len(defers) == 1 and defers[0]["content"] == "Busy — defer 30m", defers
        assert defers[0]["kind"] == {"type": "defer", "minutes": 30} and defers[0].get("via_node") is None, defers
        assert "Peer is waiting for your response." in marcus_reader.get_screen_text()
        busy_thread = thread(jun_source, "marcus", consultation, "jun-explicit-busy-pull")
        assert _defer_header_visible(busy_thread), "M30: completed thread is missing the reported-defer header\n" + busy_thread
        assert "Busy — defer 30m" in busy_thread
        close_view(marcus)
        _command(marcus, "/team reply " + consultation + " " + ANSWER)
        _wait(lambda: len(_events(workspaces["marcus"], "recipient_item_replied")) == 2, marcus.get_screen_text)
        replies = _events(workspaces["marcus"], "recipient_item_replied")
        assert replies[-1]["content"] == ANSWER and "kind" not in replies[-1] and replies[-1].get("via_node"), replies
        _wait(lambda: "Peer is waiting for your response." not in marcus_reader.get_screen_text(), marcus_reader.get_screen_text)
        answer_thread = thread(jun_source, "marcus", consultation, "jun-explicit-answer-pull")
        assert _defer_header_visible(answer_thread) and "Persist before publishing" in answer_thread, answer_thread
        _command(jun_source, "/clear")
        _wait(lambda: "thread · marcus" not in jun_source.get_screen_text(), jun_source.get_screen_text)
        _command(jun_source, "/team board marcus")
        assert jun_source.wait_for_screen(" · arrival 1", timeout=15), jun_source.get_screen_text()
        board = save(jun_source, "jun-marcus-board")
        compact_board = " ".join(board.replace("│", " ").split())
        assert f"item {consultation} · arrival 1 · task " in compact_board, board
        assert "received · 2 replies" in compact_board, board
        marcus.stop()
        marcus_reader.stop()
        _stop(daemons["marcus"])
        start_daemon("marcus", 1)
        assert _events(workspaces["marcus"], "recipient_item_replied") == replies
        writers["marcus"] = start_tui("marcus", attached=True)
        restarted_thread = thread(jun_source, "marcus", consultation, "jun-pull-after-marcus-restart")
        assert _defer_header_visible(restarted_thread) and "Persist before publishing" in restarted_thread

        local_agent = start_tui("marcus", fresh=True)
        _command(local_agent, "Update ADR-047 with the persist-before-publish decision using the ordinary local Write tool.")
        assert local_agent.wait_for_screen("[y] Allow", timeout=30), local_agent.get_screen_text()
        save(local_agent, "marcus-local-write-approval")
        local_agent.approve_permission()
        actual_adr = workspaces["marcus"] / "ADR-047.md"
        _wait(lambda: actual_adr.read_text() == ADR_EXPECTED, lambda: actual_adr.read_text())
        _wait(lambda: len(_turns(stub, "marcus")) == 2, lambda: stub.rows())
        tool_requests = _turns(stub, "marcus")
        assert tool_requests[0]["tool_results"] == 0 and "Write" in tool_requests[0]["tool_names"], tool_requests
        assert tool_requests[1]["tool_results"] == 1 and "Successfully wrote" in tool_requests[1]["last_user"], tool_requests
        assert not any(row["desync"] for row in tool_requests), tool_requests
        completed_usage = _wait(lambda: _ledger(workspaces["marcus"]), local_agent.get_screen_text)
        assert completed_usage[-1]["usage"]["tokensOut"] > 0, completed_usage
        save(local_agent, "marcus-actual-adr-write-result")
        # The native input boundary trims terminal newlines, not interior bytes.
        notification_body = actual_adr.read_text().rstrip("\n")
        _command(local_agent, "/team send --type architecture_update jun,dev2 " + notification_body)
        notification_ids = {}
        for alias in ("jun", "dev2"):
            tui = writers[alias]
            assert tui.wait_for_screen("[y] Allow once", timeout=30), tui.get_screen_text()
            save(tui, alias + "-independent-send-consent")
            tui.send("y")
            assert tui.wait_for_screen("architecture_update ·", timeout=30), tui.get_screen_text()
            notification_ids[alias] = painted_item(tui, alias + "-adr")
            body_pages = pages(tui, alias + "-adr-view")
            assert f"1 text body · {len(notification_body.encode())} bytes" in body_pages, body_pages
            assert "Persist received peer rows before publishing live cards." in body_pages, body_pages
            assert "ADR047-LIVE-PUBLISH-AFTER-PERSIST" in body_pages, body_pages
            records = _events(workspaces[alias], "recipient_item_received")
            assert len(records) == 1 and records[0]["address"]["item"] == notification_ids[alias], records
            assert records[0]["content"] == notification_body, records
            assert not _events(workspaces[alias], "recipient_item_acknowledged"), "View must not acknowledge"
            assert "peer-tainted" not in tui.get_screen_text(), "arrival/View must not select context"
            tui.send("a")
            _wait(lambda: len(_events(workspaces[alias], "recipient_item_acknowledged")) == 1, tui.get_screen_text)
            _wait(lambda: "[a] Acknowledge" not in tui.get_screen_text(), tui.get_screen_text)
            save(tui, alias + "-durable-ack")
            assert not _events(workspaces[alias], "recipient_item_replied"), "Ack must not answer"
            assert "peer-tainted" not in tui.get_screen_text(), "Ack must not select context"
            tui.send("d")
            prefix = "/team reply " + notification_ids[alias]
            assert tui.wait_for_screen(prefix, timeout=10), tui.get_screen_text()
            _command(tui, "Discussed " + alias + ": retain queue ownership.")
            _wait(lambda: len(_events(workspaces[alias], "recipient_item_replied")) == 1, tui.get_screen_text)
            assert _events(workspaces[alias], "recipient_item_replied")[0]["content"] == "Discussed " + alias + ": retain queue ownership."
        assert notification_ids["jun"] != notification_ids["dev2"], notification_ids
        def settled_send_rows():
            screen = local_agent.get_screen_text()
            return screen if all(re.search(rf"{alias}\s+awaiting their approval", screen) for alias in ("jun", "dev2")) else None
        _wait(settled_send_rows, local_agent.get_screen_text)
        send_outcomes = save(local_agent, "marcus-independent-notification-outcomes")
        for alias in ("jun", "dev2"):
            # Each notification is its own send: the first answer is that recipient's pending approval, one row each.
            assert len(re.findall(rf"(?m)^[^\n]*{alias}\s+awaiting their approval", send_outcomes)) == 1, send_outcomes
        for alias, item in notification_ids.items():
            observed = thread(local_agent, alias, item, "marcus-" + alias + "-reply-pull")
            assert "Discussed " + alias in observed, observed
            _command(local_agent, "/clear")
            _wait(lambda: "thread · " + alias not in local_agent.get_screen_text(), local_agent.get_screen_text)
            _command(local_agent, "/team board " + alias)
            assert local_agent.wait_for_screen(" · arrival 1", timeout=15), local_agent.get_screen_text()
            board = save(local_agent, "marcus-" + alias + "-board")
            compact_board = " ".join(board.replace("│", " ").split())
            assert f"item {item} · arrival 1 · task " in compact_board, board
            assert "acknowledged · 1 replies" in compact_board, board
        local_agent.stop()
        jun_source.stop()
        jun = writers["jun"]
        item = notification_ids["jun"]
        _command(jun, "/team ack " + item)
        assert jun.wait_for_screen("no new action applied", timeout=15), jun.get_screen_text()
        save(jun, "jun-idempotent-ack")
        assert len(_events(workspaces["jun"], "recipient_item_acknowledged")) == 1

        def model_probe(prompt, expected_count, expected_adr):
            before = len(_ledger(workspaces["jun"]))
            _command(jun, prompt)
            rows = _wait(lambda: _turns(stub, "jun") if len(_turns(stub, "jun")) == expected_count else None,
                         lambda: stub.rows())
            row = rows[-1]
            assert row["desync"] is False, row
            observation = row["request_observations"]
            boundary = "M19: passive View injected a received body" if prompt == "default-before-adoption" else "K10: selected body boundary"
            assert observation["needle_present"]["adr_body"] is expected_adr, f"{boundary}: ADR body presence mismatch: {row}"
            assert observation["needle_present"]["consultation_body"] is False, f"{boundary}: consultation body presence mismatch: {row}"
            _wait(lambda: len(_ledger(workspaces["jun"])) > before, jun.get_screen_text)
            assert jun.wait_for_screen("Ready", timeout=30), jun.get_screen_text()
            save(jun, prompt)

        open_view(jun, item, "jun-passive-view-before-default-probe")
        close_view(jun)
        model_probe("default-before-adoption", 1, False)
        open_view(jun, item, "jun-before-explicit-use")
        jun.send("c")
        assert jun.wait_for_screen("[c] Stop using", timeout=15), jun.get_screen_text()
        assert jun.wait_for_screen("peer-tainted", timeout=15), jun.get_screen_text()
        save(jun, "jun-selected-body-only")
        close_view(jun)
        model_probe("explicit-adoption-check", 2, True)
        open_view(jun, item, "jun-before-off")
        jun.send("c")
        assert jun.wait_for_screen("[c] Use as context", timeout=15), jun.get_screen_text()
        save(jun, "jun-off")
        close_view(jun)
        model_probe("after-off-check", 3, False)
        open_view(jun, item, "jun-reselect-for-guarded-tool")
        jun.send("c")
        assert jun.wait_for_screen("peer-tainted", timeout=15), jun.get_screen_text()
        close_view(jun)
        _command(jun, "guarded-canary-check")
        _wait(lambda: len(_turns(stub, "jun")) == 4, lambda: stub.rows())
        guarded = _turns(stub, "jun")[-1]
        assert guarded["request_observations"]["needle_present"]["adr_body"] is True, guarded
        assert "Write" in guarded["tool_names"] and guarded["tool_results"] == 0, guarded
        # Since 19.20c the attached writer receives the real approval card (exact
        # input, `[y] Allow` / `[r] Review`) instead of the old "[approval needed]
        # … approve from the local TUI" notice. Nothing decides it here: the
        # native evidence is still actual blocking/canary protection in Normal;
        # the Rust K10b suite separately proves Yolo differential + Reject.
        assert jun.wait_for_screen("[y] Allow", timeout=30), jun.get_screen_text()
        assert "Write" in jun.get_screen_text(), jun.get_screen_text()
        save(jun, "jun-selected-peer-tool-blocked")
        assert canary.read_text() == "unchanged native canary\n"
        _stop(daemons["jun"])
        assert canary.read_text() == "unchanged native canary\n"
        assert _sha(binary) == binary_sha, "binary changed during native capture"
        assert all(not row.get("desync", False) for row in stub.rows()), stub.rows()
        assert len(_turns(stub, "marcus")) == 2 and len(_turns(stub, "jun")) == 4 and not _turns(stub, "dev2")
        logs = {}
        for alias, workspace in workspaces.items():
            result = subprocess.run([str(binary), "team", "log", "--json"], cwd=workspace,
                                    env=environments[alias], text=True, capture_output=True, timeout=30, check=True)
            json.loads(result.stdout)
            logs[alias] = result.stdout
            (root / (alias + ".team-log.json")).write_text(result.stdout)
        manifest = {
            "binary": str(binary), "binary_sha256": binary_sha, "binary_bytes": binary.stat().st_size,
            "source_sha256": _sha(Path(__file__)), "scene_sha256": _sha(SCENE), "fixture_sha256": _sha(ADR),
            "columns": columns, "painted_consultation_id": consultation, "painted_notification_ids": notification_ids,
            "actual_provider_turns": {"marcus": 2, "jun": 4, "dev2": 0}, "actual_adr_sha256": _sha(actual_adr),
            "snapshots": snapshots, "raw_root": str(root), "normalization": "Raw terminal/screens/journals unchanged. Header assertions ignore cell-wrap whitespace/borders only. Notification carrier deliberately omits the ADR file's terminal newline, matching the existing native input boundary; timestamps and allocated IDs are not normalized.",
            "limits": "Loopback principal collapse; pull-only replies; fixed untracked 30m; one text carrier; operator-approved independent sends. Native Normal-mode blocked tool is not differential taint enforcement or an invented attached Reject UI; actual K10b covers differential/Reject. Not agent-mediated 19.26."
        }
        (root / "identity.json").write_text(json.dumps(manifest, indent=2) + "\n")
        receipts = Path(__file__).parent / "journeys/j10/receipts" / identity
        receipts.mkdir(parents=True, exist_ok=False)
        receipt = receipts / f"story-19-19-act2-{stamp}.transcript.txt"
        with receipt.open("x") as stream:
            sections = [f"recorded-at: {recorded}\nscene: tests_tui/journeys/j10/scenes/19-19-act2-v1.json sha256: {_sha(SCENE)}\n"
                        f"source: tests_tui/test_story_19_19_act2_consultation.py sha256: {_sha(Path(__file__))}\n",
                        json.dumps(manifest, indent=2)]
            for alias, text in logs.items():
                sections.append(f"\n--- actual {alias} product team log --json ---\n{text}")
            for path in sorted(root.glob("*.terminal.txt")):
                sections.append(f"\n--- raw {path.name} ---\n{path.read_text()}")
            stream.write("\n".join(sections))
        (root / "receipt-path.txt").write_text(str(receipt) + "\n")
        print(f"ACT2_RECEIPT={receipt}", flush=True)
