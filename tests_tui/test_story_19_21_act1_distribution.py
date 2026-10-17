"""Story 19.21 AC6: real attached Act 1 distribution through the native PTYs."""
from __future__ import annotations

from contextlib import ExitStack
from datetime import datetime, timezone
import hashlib
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
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import threading

import pytest

import harness
from harness import RustainTUI
from fixtures.scene_provider import SceneStub

pytestmark = [pytest.mark.story_19_21, pytest.mark.a2a_binary]
SCENE = Path(__file__).parent / "journeys/j10/scenes/19-21-act1-v1.json"
PERSONAS = {
    "lena-po": "scene-lena-po",
    "jun-dev": "scene-jun-dev",
    "tom-dev": "scene-tom-dev",
    "yuki-dev": "scene-yuki-dev",
    "nina-ux": "scene-nina-ux",
}
STORY = (
    "ACT1-STORY-19-21\n"
    "Story: route ownership through the distribution rail.\n"
    "Acceptance: preserve lines; do not auto-ack.\n"
    "Evidence: ACT1-VIEW-SENTINEL."
)


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

class _HeldPostProxy:
    """HTTP forwarder that holds the first real A2A message/send before backend."""

    def __init__(self, backend_port):
        self.backend_port = backend_port
        self.entered = threading.Event()
        self.release = threading.Event()
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                self._forward()

            def do_POST(self):
                length = int(self.headers.get("Content-Length", "0"))
                body = self.rfile.read(length)
                try:
                    method = json.loads(body).get("method")
                except (UnicodeDecodeError, json.JSONDecodeError, AttributeError):
                    method = None
                if method in ("message/send", "tasks/send"):
                    owner.entered.set()
                    owner.release.wait()
                    # Cleanup closes the killed sender's socket without forwarding the POST.
                    self.close_connection = True
                    return
                self._forward(body)

            def _forward(self, body=None):
                headers = {
                    key: value for key, value in self.headers.items()
                    if key.lower() not in {"host", "connection", "content-length"}
                }
                request = urllib.request.Request(
                    f"http://127.0.0.1:{owner.backend_port}{self.path}",
                    data=body, headers=headers, method=self.command,
                )
                try:
                    response = urllib.request.urlopen(request, timeout=30)
                except urllib.error.HTTPError as error:
                    response = error
                except (OSError, urllib.error.URLError) as error:
                    self.send_error(502, str(error))
                    return
                payload = response.read()
                self.send_response(response.status)
                for key, value in response.headers.items():
                    if key.lower() not in {"connection", "transfer-encoding", "content-length"}:
                        self.send_header(key, value)
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def log_message(self, *_):
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.port = self.server.server_address[1]
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def stop(self):
        self.release.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)


def _command(tui, text):
    # Embedded newlines are one bracketed paste and the only Enter submits it.
    tui.send("\x1b[200~" + text + "\x1b[201~")
    tui.send("\r")


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


def _turns(stub, alias):
    return [row for row in stub.rows() if row.get("persona") == PERSONAS[alias] and "turn" in row]


def _provider_requests(stub, alias):
    return [row for row in stub.rows()
            if row.get("persona") == PERSONAS[alias]
            and row.get("path") == "/v1/messages"
            and (row.get("stream") is True or row.get("kind") == "title")]


def _compact(text):
    return " ".join(text.replace("│", " ").split())


def _painted_item(tui, alias, snapshots):
    tui.send("\t")
    assert tui.wait_for_screen("item actions ·", timeout=15), tui.get_screen_text()
    screen = tui.get_screen_text()
    snapshots.append((alias + "-painted-item", screen))
    match = re.search(r"item actions · (ri_[A-Za-z0-9_-]+)", screen)
    assert match, screen
    return match.group(1)
def _surface_pages(tui, name, snapshots, limit=12):
    pages = [tui.get_screen_text()]
    snapshots.append((f"{name}-initial", pages[0]))
    previous = None
    for _ in range(limit):
        tui.send("\x1b[5~")
    for index in range(limit * 2):
        text = tui.get_screen_text()
        snapshots.append((f"{name}-{index}", text))
        if text == previous:
            break
        pages.append(text)
        previous = text
        tui.send("\x1b[6~")
    else:
        raise AssertionError(f"{name}: recipient surface pagination did not reach its end")
    return "\n".join(pages)




def _chat_pages(tui, name, snapshots, limit=20):
    # Attached PageUp/PageDown use the last painted viewport, capped at five rows.
    # Capture each position after the event loop redraws; keep sending at a
    # boundary too, so a repeated blank screen cannot leave the composer scrolled.
    # Compare row order only in the top-to-bottom pass; retain reverse-scroll evidence too.
    initial = tui.get_screen_text()
    snapshots.append((f"{name}-start", initial))

    def move(key, index):
        previous = tui.get_screen_text()
        tui.send(key)
        try:
            current = _wait(
                lambda: (screen if screen != previous else None)
                if (screen := tui.get_screen_text()) else None,
                tui.get_screen_text,
                timeout=0.2,
            )
        except AssertionError:
            current = tui.get_screen_text()
        snapshots.append((f"{name}-{index}", current))
        return current

    for index in range(limit):
        move("\x1b[5~", index)
    ordered_pages = [tui.get_screen_text()]
    for index in range(limit):
        ordered_pages.append(move("\x1b[6~", limit + index))
    return "\n".join(ordered_pages)


def _wait_for_marker(tui, marker, diagnostic):
    moves = 0

    def visible():
        nonlocal moves
        screen = tui.get_screen_text()
        if marker in _compact(screen):
            # Undo only our navigation; PageUp's stored offset is not top-clamped.
            for _ in range(min(moves, 40 - moves)):
                tui.send("\x1b[6~")
            return screen
        # Balanced cycles revisit both ends when the response arrives late.
        # No screen-equality boundary guess: an overscrolled PageDown can look unchanged.
        tui.send("\x1b[5~" if moves < 20 else "\x1b[6~")
        moves = (moves + 1) % 40
        return None

    return _wait(visible, diagnostic)


def _fresh_board(tui, command, marker, name, snapshots, clear_command=None, clear_marker=None):
    # A repeated selector may retain its previous stable FeedbackBlock. Require
    # the intervening selector's rendered product content before issuing it.
    if clear_command:
        _command(tui, clear_command)
        cleared = _wait_for_marker(tui, clear_marker, tui.get_screen_text)
        snapshots.append((name + "-clear-observed", cleared))
        cleared_pages = _chat_pages(tui, name + "-clear", snapshots)
        assert clear_marker in _compact(cleared_pages), cleared_pages
    _command(tui, command)
    observed = _wait_for_marker(tui, marker, tui.get_screen_text)
    snapshots.append((name + "-observed", observed))
    rendered = _chat_pages(tui, name, snapshots)
    assert marker in _compact(rendered), rendered
    return rendered


@pytest.mark.parametrize("columns,rows", [(80, 24), (130, 30), (60, 16)], ids=["80x24", "130x30", "60x16"])
@pytest.mark.timeout(420)
def test_act1_attached_distribution(columns, rows, monkeypatch):
    binary_name = os.environ.get("RUSTAIN_TUI_BINARY")
    assert binary_name, "Act 1 requires an explicitly built four-feature binary"
    binary = Path(binary_name).resolve()
    assert binary.is_file(), binary
    client_binary_name = os.environ.get("RUSTAIN_TUI_CLIENT_BINARY")
    client_binary = Path(client_binary_name).resolve() if client_binary_name else None
    if client_binary is not None:
        assert client_binary.is_file(), client_binary
        monkeypatch.setenv("RUSTAIN_TUI_CLIENT_BINARY", str(client_binary))
    root = Path(tempfile.mkdtemp(dir="/tmp", prefix=f"r19-21-act1-{columns}-"))
    identity = f"{columns}x{rows}-{datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%S%fZ')}-{uuid.uuid4().hex}"
    evidence_root = Path(os.environ.get(
        "RUSTAIN_CAPTURE_EVIDENCE_DIR",
        Path(__file__).parent / "journeys/j10/receipts/evidence",
    )) / identity
    evidence_root.mkdir(parents=True, exist_ok=False)
    assert len(str(root / "lena-po" / ".rustain_data" / "daemons" / ("a" * 16 + ".sock"))) <= 100
    binary_sha = _sha(binary)
    monkeypatch.setenv("RUSTAIN_TUI_BINARY", str(binary))
    monkeypatch.setattr(harness, "TERM_COLS", columns)
    monkeypatch.setattr(harness, "TERM_ROWS", rows)
    stamp_time = datetime.now(timezone.utc).replace(microsecond=0)
    stamp = stamp_time.strftime("%Y-%m-%dT%H-%M-%SZ")
    recorded = stamp_time.strftime("%Y-%m-%dT%H:%M:%SZ")
    snapshots = []
    resources = ExitStack()

    def retain_evidence():
        for path in root.rglob("*"):
            if not path.is_file():
                continue
            if (path.name == "requests.jsonl" or path.name.endswith((".screen.txt", ".daemon.log", ".runtime.log", ".team-log.json", ".terminal.txt"))
                    or path.name.endswith(".jsonl") and ".rustain/rooms" in path.as_posix()):
                target = evidence_root / path.relative_to(root)
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(path, target)
        receipt_dir = Path(__file__).parent / "journeys/j10/receipts" / identity
        if receipt_dir.is_dir():
            shutil.copytree(receipt_dir, evidence_root / "receipt", dirs_exist_ok=True)

    resources.callback(retain_evidence)
    with resources:
        stub = resources.enter_context(SceneStub(SCENE, root / "requests.jsonl"))
        aliases = tuple(PERSONAS)
        workspaces = {alias: root / alias for alias in aliases}
        homes = {alias: root / (alias + "-home") for alias in aliases}
        ports, reservations = {}, {}
        for alias in aliases:
            workspaces[alias].mkdir()
            homes[alias].mkdir()
            reservation = socket.socket()
            reservation.bind(("127.0.0.1", 0))
            ports[alias] = reservation.getsockname()[1]
            reservations[alias] = reservation

        def release_reservations():
            for reservation in reservations.values():
                reservation.close()
            reservations.clear()

        resources.callback(release_reservations)
        endpoints = {alias: f"http://127.0.0.1:{port}/" for alias, port in ports.items()}
        environments, daemons = {}, {}
        for alias, workspace in workspaces.items():
            config = workspace / ".rustain"
            config.mkdir()
            (config / "a2a.json").write_text(json.dumps({
                "server": {"admission": "deny" if alias == "yuki-dev" else "allow"},
                "agents": {peer: {"url": endpoint} for peer, endpoint in endpoints.items() if peer != alias},
            }))
            (config / "a2a-interaction.toml").write_text(
                '[interaction.defaults]\nresponse_mode = "notify-and-wait"\nnotification = "immediate"\n')
            (config / "config.toml").write_text('[permissions]\nalways_tools = ["Read"]\n')
            environments[alias] = {
                **os.environ, **stub.env(PERSONAS[alias], homes[alias]),
                "TERM": "xterm-256color", "RUST_LOG": "debug", "RUSTAIN_CONFIG_DIR": str(config),
                "RUSTAIN_DATA_DIR": str(workspace / ".rustain_data"),
                "RUSTAIN_LOG_PATH": str(root / (alias + ".runtime.log")),
            }

        def start_daemon(alias):
            log_path = root / f"{alias}.daemon.log"
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

        for alias in ("jun-dev", "tom-dev", "yuki-dev"):
            start_daemon(alias)
        # Lena's egress is composed only after listening peers are live; observe its
        # successful boot catalog events rather than sleeping through discovery.
        start_daemon("lena-po")

        def sender_cards_ready():
            logs = list(root.glob("lena-po.runtime.log*"))
            text = "\n".join(path.read_text() for path in logs)
            return text if all("A2aCatalogChanged" in text and f"peer_id={peer}" in text
                               for peer in ("jun-dev", "tom-dev", "yuki-dev")) else None

        _wait(sender_cards_ready, lambda: "\n".join(path.read_text() for path in root.glob("lena-po.runtime.log*")))
        # Nina's socket stays bound but never listens: a real roster entry with no daemon.

        sessions = iter(range(1000))

        def start_tui(alias):
            tui = resources.enter_context(RustainTUI(
                fresh=False, build=False, workspace=workspaces[alias], allowed_tools=["Read"],
                extra_args=["daemon", "attach"],
                binary=client_binary,
                env_overrides=stub.env(PERSONAS[alias], homes[alias]), timeout=30))
            raw = root / f"{alias}-attached-{next(sessions)}.terminal.txt"
            tui.child.logfile_read = resources.enter_context(raw.open("w"))
            assert tui.wait_for_screen("Ready", timeout=30), tui.get_screen_text()
            return tui
        lena, jun, tom = (start_tui(alias) for alias in ("lena-po", "jun-dev", "tom-dev"))
        _command(lena, "/team send --type story_assignment --subject sprint-7 jun-dev,tom-dev,yuki-dev,nina-ux " + STORY)

        for recipient in (jun, tom):
            assert recipient.wait_for_screen("[y] Allow once", timeout=45), recipient.get_screen_text()
            recipient.send("y")
            assert recipient.wait_for_screen("story_assignment ·", timeout=45), recipient.get_screen_text()
        sender_feedback_rows = (
            "jun-dev awaiting their approval",
            "tom-dev awaiting their approval",
            "✗ yuki-dev declined",
            "⚠ nina-ux unreachable",
        )
        feedback_diagnostic = (
            "R15: original Lena send feedback must contain all four terminal outcomes "
            "in operator order and no sending… before first board read\n"
        )
        # Journal settlements are independent of the client's feedback-row
        # consumer, so M59 cannot pass by timing out while application is disabled.
        def durable_send_settlements():
            events = _events(workspaces["lena-po"], "team_send_recipient_settled")
            return events if len(events) == 4 else None

        send_settlements = _wait(
            durable_send_settlements,
            lambda: feedback_diagnostic + lena.get_screen_text(),
        )
        assert len(send_settlements) == 4, send_settlements
        # First answers require consent; later board reads refine those durable
        # send-time outcomes after the recipient approves. A separate status
        # response fences UI progress independently of settlement-row application.
        # Its final peer row remains painted even when the header is above a short viewport.
        _command(lena, "/team status")
        assert lena.wait_for_screen("- yuki-dev", timeout=30), lena.get_screen_text()
        lena_send_feedback = _chat_pages(lena, "lena-send-feedback", snapshots)
        compact_send_feedback = _compact(lena_send_feedback)
        feedback_positions = [compact_send_feedback.find(row) for row in sender_feedback_rows]
        assert all(position >= 0 for position in feedback_positions), (
            feedback_diagnostic + lena_send_feedback
        )
        assert feedback_positions == sorted(feedback_positions), (
            feedback_diagnostic + lena_send_feedback
        )
        assert "sending…" not in compact_send_feedback, (
            feedback_diagnostic + lena_send_feedback
        )
        jun_item = _painted_item(jun, "jun-dev", snapshots)
        tom_item = _painted_item(tom, "tom-dev", snapshots)
        assert jun_item != tom_item, (jun_item, tom_item)
        view = _surface_pages(jun, "jun-view", snapshots)
        compact_story = _compact(STORY)
        assert compact_story in _compact(view), view
        assert "1 text body · " + str(len(STORY.encode())) + " bytes" in view, view
        assert not _events(workspaces["jun-dev"], "recipient_item_acknowledged"), "View must not acknowledge"

        last_pending = _fresh_board(lena, "/team board --last", "Distribution board · story_assignment",
                                    "lena-last-pending", snapshots)
        compact_pending = _compact(last_pending)
        assert "● jun-dev" in compact_pending, compact_pending
        jun.send("a")
        _wait(lambda: len(_events(workspaces["jun-dev"], "recipient_item_acknowledged")) == 1, jun.get_screen_text)
        logs, product_logs = {}, {}
        for alias, workspace in workspaces.items():
            result = subprocess.run([str(binary), "team", "log", "--json"], cwd=workspace,
                                    env=environments[alias], text=True, capture_output=True,
                                    timeout=30, check=True)
            product_logs[alias] = json.loads(result.stdout)
            logs[alias] = result.stdout
            (root / (alias + ".team-log.json")).write_text(result.stdout)
        jun_kinds = [row.get("kind") for row in product_logs["jun-dev"]["rows"]]
        tom_kinds = [row.get("kind") for row in product_logs["tom-dev"]["rows"]]
        assert "item-received" in jun_kinds and "item-acknowledged" in jun_kinds, product_logs["jun-dev"]
        assert "item-received" in tom_kinds and "item-acknowledged" not in tom_kinds, product_logs["tom-dev"]
        last_acked = _fresh_board(
            lena, "/team board --last", "Distribution board · story_assignment",
            "lena-last-acked", snapshots, clear_command="/team board",
            clear_marker="Peer item inventory",
        )
        compact_acked = _compact(last_acked)
        assert "✓ jun-dev" in compact_acked, compact_acked

        final_board = last_acked
        compact_final = compact_acked
        assert "sprint-7" in compact_final, compact_final
        assert re.search(r"attempted .+ · 4 addressed", compact_final), "M53: final board header must include its exact addressed count\n" + final_board
        board_rows = compact_final.split("Distribution board · story_assignment", 1)[1]
        order = [board_rows.find(token) for token in ("✓ jun-dev", "● tom-dev", "✗ yuki-dev", "⚠ nina-ux")]
        assert all(index >= 0 for index in order), "M55: 60-column board must preserve every outcome row\n" + final_board
        disclosure = ("⚠ At least one peer answered over a loopback bind, where every local caller is "
                      "the same principal — the rows above are that host's items for all local callers, "
                      "not this caller's alone.")
        assert _compact(disclosure) in compact_final, "M55: 60-column board must preserve the full collapse disclosure\n" + final_board
        assert order == sorted(order), board_rows
        assert "aggregate" not in compact_final.lower(), compact_final
        inventory = _fresh_board(lena, "/team board", "Peer item inventory", "lena-inventory", snapshots)
        assert re.search(r"Peer item inventory · \d+ rows", _compact(inventory)), inventory
        assert "Distribution board · story_assignment" not in inventory, inventory

        thread_refusal = "'/team thread' unavailable in this session.\n    Run it in a non-attached session."
        before_thread_requests = _provider_requests(stub, "lena-po")
        before_thread_turns = _turns(stub, "lena-po")
        _command(lena, f"/team thread jun-dev {jun_item}")
        thread_render = _chat_pages(lena, "lena-thread-refusal", snapshots)
        thread_compact = _compact(thread_render)
        assert _compact(thread_refusal) in thread_compact, thread_render
        assert "ℹ '/team thread' unavailable in this session." in thread_compact, thread_render
        assert _provider_requests(stub, "lena-po") == before_thread_requests, stub.rows()
        assert _turns(stub, "lena-po") == before_thread_turns, stub.rows()

        status_commands = [("/team status", "lena-status")]
        if columns == 130:
            status_commands.append(("/team trust", "lena-trust"))
        for command, name in status_commands:
            before_requests = _provider_requests(stub, "lena-po")
            before_turns = _turns(stub, "lena-po")
            _command(lena, command)
            status_render = _chat_pages(lena, name, snapshots)
            status_compact = _compact(status_render)
            assert "ℹ Team interaction policy" in status_compact, status_render
            assert "Response mode: notify-and-wait" in status_compact, status_render
            assert "Notification urgency: immediate" in status_compact, status_render
            assert "Peers:" in status_compact, status_render
            for alias in ("jun-dev", "tom-dev", "yuki-dev", "nina-ux"):
                assert f"- {alias} (" in status_compact, status_render
            assert _compact(thread_refusal) in status_compact, "S7 Info refusal must persist after the status read\n" + status_render
            assert _provider_requests(stub, "lena-po") == before_requests, stub.rows()
            assert _turns(stub, "lena-po") == before_turns, stub.rows()

        for alias in aliases:
            assert not _turns(stub, alias), f"{alias}: application provider turn unexpectedly issued: {stub.rows()}"
        for alias in ("jun-dev", "tom-dev", "yuki-dev", "nina-ux"):
            assert not _provider_requests(stub, alias), f"{alias}: recipient provider request unexpectedly issued: {stub.rows()}"
        assert not _events(workspaces["tom-dev"], "recipient_item_acknowledged")
        assert not _events(workspaces["yuki-dev"], "recipient_item_received")
        assert not _events(workspaces["nina-ux"], "recipient_item_received")
        jun_acks = _events(workspaces["jun-dev"], "recipient_item_acknowledged")
        assert len(jun_acks) == 1

        assert _sha(binary) == binary_sha, "capture daemon binary changed during native run"
        for name, text in snapshots:
            (root / (name + ".screen.txt")).write_text(text)
        manifest = {
            "daemon_binary": str(binary), "daemon_binary_sha256": binary_sha,
            "daemon_binary_bytes": binary.stat().st_size,
            "client_binary": str(client_binary or binary),
            "client_binary_sha256": _sha(client_binary or binary),
            "client_binary_bytes": (client_binary or binary).stat().st_size,
            "daemon_protocol_version": 7,
            "source_sha256": _sha(Path(__file__)), "scene_sha256": _sha(SCENE),
            "columns": columns, "rows": rows, "jun_item_from_painted_surface": jun_item,
            "tom_item_from_painted_surface": tom_item, "provider_turns": {alias: len(_turns(stub, alias)) for alias in aliases},
            "sender_feedback": sender_feedback_rows,
            "sender_feedback_asserted_before_first_board": True,
            "recipient_application_provider_requests": {alias: _provider_requests(stub, alias) for alias in aliases[1:]},
            "snapshots": [name for name, _ in snapshots],
            "limits": "Native attached distribution only; loopback aliases share one local principal. No J10/gate claim.",
        }
        (root / "identity.json").write_text(json.dumps(manifest, indent=2) + "\n")
        receipt_dir = Path(__file__).parent / "journeys/j10/receipts" / identity
        receipt_dir.mkdir(parents=True, exist_ok=False)
        receipt = receipt_dir / f"story-19-21-act1-{stamp}.transcript.txt"
        with receipt.open("x") as stream:
            sections = [f"recorded-at: {recorded}\nscene: tests_tui/journeys/j10/scenes/19-21-act1-v1.json sha256: {_sha(SCENE)}\n"
                        f"source: tests_tui/test_story_19_21_act1_distribution.py sha256: {_sha(Path(__file__))}\n",
                        json.dumps(manifest, indent=2)]
            for alias, text in logs.items():
                sections.append(f"\n--- actual {alias} product team log --json ---\n{text}")
            for name, text in snapshots:
                sections.append(f"\n--- actual painted {name} ---\n{text}")
            stream.write("\n".join(sections))
        (root / "receipt-path.txt").write_text(str(receipt) + "\n")
        print(f"ACT1_RECEIPT={receipt}", flush=True)


@pytest.mark.timeout(420)
def test_act1_daemon_crash_restart(monkeypatch):
    """Kill the actual sender daemon while a signed-peer message/send POST is held."""
    binary_name = os.environ.get("RUSTAIN_TUI_BINARY")
    assert binary_name, "R14 requires an explicitly built four-feature daemon binary"
    binary = Path(binary_name).resolve()
    assert binary.is_file(), binary
    client_name = os.environ.get("RUSTAIN_TUI_CLIENT_BINARY")
    client_binary = Path(client_name).resolve() if client_name else binary
    assert client_binary.is_file(), client_binary
    root = Path(tempfile.mkdtemp(dir="/tmp", prefix="r19-21-daemon-restart-"))
    identity = f"{datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%S%fZ')}-{uuid.uuid4().hex}"
    evidence_root = Path(os.environ.get(
        "RUSTAIN_CAPTURE_EVIDENCE_DIR",
        Path(__file__).parent / "journeys/j10/receipts/evidence",
    )) / identity
    evidence_root.mkdir(parents=True, exist_ok=False)
    binary_sha, client_sha = _sha(binary), _sha(client_binary)
    monkeypatch.setenv("RUSTAIN_TUI_BINARY", str(binary))
    snapshots = []
    resources = ExitStack()

    def retain_evidence():
        for path in root.rglob("*"):
            if path.is_file() and (
                path.name.endswith((".screen.txt", ".daemon.log", ".runtime.log", ".terminal.txt"))
                or path.name.endswith(".jsonl") and ".rustain/rooms" in path.as_posix()
            ):
                target = evidence_root / path.relative_to(root)
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(path, target)

    resources.callback(retain_evidence)
    with resources:
        stub = resources.enter_context(SceneStub(SCENE, root / "requests.jsonl"))
        aliases = ("lena-po", "jun-dev", "tom-dev")
        workspaces = {alias: root / alias for alias in aliases}
        homes = {alias: root / (alias + "-home") for alias in aliases}
        reservations = {}
        ports = {}
        for alias in aliases:
            workspaces[alias].mkdir()
            homes[alias].mkdir()
            reservation = socket.socket()
            reservation.bind(("127.0.0.1", 0))
            ports[alias] = reservation.getsockname()[1]
            reservations[alias] = reservation
        for reservation in reservations.values():
            reservation.close()

        gate = _HeldPostProxy(ports["tom-dev"])
        resources.callback(gate.stop)
        endpoints = {
            "lena-po": f"http://127.0.0.1:{ports['lena-po']}/",
            "jun-dev": f"http://127.0.0.1:{ports['jun-dev']}/",
            "tom-dev": f"http://127.0.0.1:{gate.port}/",
        }
        environments = {}
        for alias, workspace in workspaces.items():
            config = workspace / ".rustain"
            config.mkdir()
            server = {"admission": "allow"}
            if alias == "tom-dev":
                server["advertised_host"] = f"127.0.0.1:{gate.port}"
            peers = {peer: {"url": endpoints[peer]} for peer in aliases if peer != alias}
            (config / "a2a.json").write_text(json.dumps({"server": server, "agents": peers}))
            (config / "a2a-interaction.toml").write_text(
                '[interaction.defaults]\nresponse_mode = "notify-and-wait"\nnotification = "immediate"\n'
            )
            (config / "config.toml").write_text('[permissions]\nalways_tools = ["Read"]\n')
            environments[alias] = {
                **os.environ, **stub.env(PERSONAS[alias], homes[alias]),
                "TERM": "xterm-256color", "RUST_LOG": "debug", "RUSTAIN_CONFIG_DIR": str(config),
                "RUSTAIN_DATA_DIR": str(workspace / ".rustain_data"),
                "RUSTAIN_LOG_PATH": str(root / (alias + ".runtime.log")),
            }

        daemons = {}

        def start_daemon(alias, generation):
            log_path = root / f"{alias}.daemon-{generation}.log"
            log = resources.enter_context(log_path.open("w"))
            process = subprocess.Popen(
                [str(binary), f"--serve-a2a=127.0.0.1:{ports[alias]}", "daemon", "start", "--foreground"],
                cwd=workspaces[alias], env=environments[alias], stdout=log,
                stderr=subprocess.STDOUT, start_new_session=True,
            )
            resources.callback(_stop, process)
            daemons[alias] = process
            ready_url = endpoints[alias]

            def ready():
                assert process.poll() is None, log_path.read_text()
                try:
                    with urllib.request.urlopen(ready_url + ".well-known/agent-card.json", timeout=1) as reply:
                        return reply.status == 200
                except (OSError, urllib.error.URLError):
                    return False

            _wait(ready, log_path.read_text)

        start_daemon("jun-dev", 0)
        start_daemon("tom-dev", 0)
        start_daemon("lena-po", 0)

        def cards_ready():
            text = "\n".join(path.read_text() for path in root.glob("lena-po.runtime.log*"))
            return text if all(
                "A2aCatalogChanged" in text and f"peer_id={peer}" in text
                for peer in ("jun-dev", "tom-dev")
            ) else None

        _wait(cards_ready, lambda: "\n".join(path.read_text() for path in root.glob("lena-po.runtime.log*")))

        sessions = iter(range(10))

        def start_tui(alias, generation):
            tui = resources.enter_context(RustainTUI(
                fresh=False, build=False, workspace=workspaces[alias], allowed_tools=["Read"],
                extra_args=["daemon", "attach"], binary=client_binary,
                env_overrides=stub.env(PERSONAS[alias], homes[alias]), timeout=30,
            ))
            raw = root / f"{alias}-attached-{generation}-{next(sessions)}.terminal.txt"
            tui.child.logfile_read = resources.enter_context(raw.open("w"))
            assert tui.wait_for_screen("Ready", timeout=30), tui.get_screen_text()
            return tui

        lena = start_tui("lena-po", 0)
        jun = start_tui("jun-dev", 0)
        _command(lena, "/team send --type story_assignment --subject restart-check tom-dev,jun-dev " + STORY)
        assert jun.wait_for_screen("[y] Allow once", timeout=45), jun.get_screen_text()
        jun.send("y")
        assert jun.wait_for_screen("story_assignment ·", timeout=45), jun.get_screen_text()
        jun_item = _painted_item(jun, "jun-dev", snapshots)
        jun.send("a")
        jun_ack = _wait(
            lambda: _events(workspaces["jun-dev"], "recipient_item_acknowledged"),
            jun.get_screen_text,
        )
        assert len(jun_ack) == 1, jun_ack
        assert gate.entered.wait(timeout=60), (
            "R14: real signed-peer message/send POST never reached deterministic hold\n"
            + "\n".join(path.read_text() for path in root.glob("lena-po.runtime.log*"))
        )
        received = _wait(
            lambda: _events(workspaces["jun-dev"], "recipient_item_received"),
            jun.get_screen_text,
        )
        assert len(received) == 1, received
        landed_task = received[0].get("task")
        assert landed_task, received
        action_records = _events(workspaces["lena-po"], "team_send_action_recorded")
        assert len(action_records) == 1, action_records

        sender_process = daemons["lena-po"]
        os.killpg(sender_process.pid, signal.SIGKILL)
        sender_process.wait(timeout=10)
        assert sender_process.returncode == -signal.SIGKILL, sender_process.returncode
        lena.stop()
        start_daemon("lena-po", 1)
        # A new attach submits the product board request through the restarted Unix socket.
        lena_after = start_tui("lena-po", 1)
        _command(lena_after, "/team board --last")
        board = _wait_for_marker(
            lena_after, "Distribution board · story_assignment", lena_after.get_screen_text,
        )
        board_pages = _chat_pages(lena_after, "restart-board", snapshots)
        compact_board = _compact(board_pages)
        assert "tom-dev" in compact_board and "no outcome recorded" in compact_board, board_pages
        assert "jun-dev" in compact_board and ("✓ jun-dev" in compact_board or "● jun-dev" in compact_board), board_pages
        assert "sending…" not in compact_board, board_pages
        assert landed_task in board_pages, (
            "R14: landed item task correlation must be visible in restarted action board\n"
            + board_pages
        )
        snapshots.append(("restart-board-observed", board))
        assert _sha(binary) == binary_sha and _sha(client_binary) == client_sha
        manifest = {
            "case": "test_act1_daemon_crash_restart",
            "daemon_binary": str(binary), "daemon_binary_sha256": binary_sha,
            "client_binary": str(client_binary), "client_binary_sha256": client_sha,
            "daemon_protocol_version": 7,
            "held_post_observed_before_os_kill": True,
            "sender_exit_code": sender_process.returncode,
            "held_recipient": "tom-dev", "landed_recipient": "jun-dev",
            "landed_task_correlation": landed_task, "landed_item": jun_item,
            "board_assertions": ["tom-dev no outcome recorded", "jun-dev delivered/acknowledged", "no sending…"],
            "snapshots": [name for name, _ in snapshots],
        }
        for name, text in snapshots:
            (root / (name + ".screen.txt")).write_text(text)
        (evidence_root / "identity.json").write_text(json.dumps(manifest, indent=2) + "\n")
        print(f"ACT1_DAEMON_RESTART_RECEIPT={evidence_root / 'identity.json'}", flush=True)
