"""Original AC6/K07: the three-host, operator-started architecture finding."""
from __future__ import annotations

import base64
from contextlib import ExitStack
from datetime import datetime, timezone
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import socket
import ssl
import subprocess
import tempfile
import unicodedata
import urllib.error
import urllib.request
import uuid

import pytest
import pyte

import harness
from harness import RustainTUI
from conflict_receipt_manifest import begin_run
from fixtures.scene_provider import SceneStub
from test_story_19_21_act1_distribution import (
    _chat_pages, _command, _compact, _events, _fresh_board, _painted_item,
    _sha, _stop, _surface_pages, _wait, _wait_for_marker,
)
from test_story_19_21b_standalone_modes import _ObservedHttp

pytestmark = [pytest.mark.story_19_21, pytest.mark.a2a_binary]
SCENE = Path(__file__).parent / "journeys/j10/scenes/19-21b-conflict-flag-v1.json"
SOURCE = Path(__file__).resolve()
RECEIPTS = Path(__file__).parent / "journeys/j10/receipts"
PERSONAS = {alias: "scene-" + alias for alias in ("lena-po", "jun-dev", "marcus-arch")}
RECIPIENTS = ("lena-po", "jun-dev")
STORY = (
    "STORY-047: publish the received card before persisting its row.\n"
    "Jun owns implementation; Marcus checks the architecture.\n"
    "Acceptance: an arriving item is visible before its durable append.\n"
    "Please identify conflicts with ADR-047; do not send automatically."
)
HEADING = "Your agent proposes a message. Nothing has been sent. Awaiting your decision."
ALIASES = "To: lena-po, jun-dev"
TYPE = "Type: architecture_update"
FOOTER = "[y] Send to lena-po, jun-dev   [r] Review first   [n] Dismiss"
EVENT_NAMES = (
    "team_send_action_recorded", "team_send_recipient_settled",
    "remote_envelope_dispatched", "recipient_item_received",
    "recipient_item_acknowledged", "recipient_item_replied",
)


@pytest.fixture(scope="session")
def conflict_run(request):
    binary_name = os.environ.get("RUSTAIN_TUI_BINARY")
    assert binary_name, "conflict capture requires an explicitly built four-feature release binary"
    binary = Path(binary_name).resolve()
    viewports = [
        f"{item.callspec.params['columns']}x{item.callspec.params['rows']}"
        for item in request.session.items
        if item.path.resolve() == SOURCE
        and item.originalname == "test_conflict_flag_capture"
    ]
    return begin_run(SOURCE.parent.parent, binary, viewports)


def _painted_rows(text, width):
    # Reuse the harness's terminal painter as an independent cell-width oracle,
    # rather than implementing the product's grapheme/whitespace wrapper again.
    text = unicodedata.normalize("NFC", text)
    terminal = pyte.Screen(width, len(text) + 2)
    pyte.Stream(terminal).feed(text.replace("\n", "\r\n"))
    return [row.rstrip(" ") for row in terminal.display[:terminal.cursor.y + 1]]


def _review_rows(rendered, screen_height):
    lines = rendered.splitlines()
    assert len(lines) % screen_height == 0, "incomplete native Review page"
    pages = [lines[start:start + screen_height] for start in range(0, len(lines), screen_height)]
    merged = []
    previous = None
    for page in pages:
        # Review reserves the whole transcript above its double-bordered card.
        # Never collect overlay aliases, quoted body chrome or composer rows.
        boundary = next(i for i, row in enumerate(page) if "╔" in row)
        current = [unicodedata.normalize("NFC", row.rstrip(" ")) for row in page[:boundary]]
        assert current, "native Review has no scrollable transcript"
        if previous is None:
            merged.extend(current)
        else:
            assert len(current) == len(previous), "native Review viewport changed during pagination"
            step = min(len(current), 5)
            # PageDown advances at most five rows. Prefer its full, known stride;
            # only a clamped boundary may have a larger observed overlap.
            overlap = len(current) - step
            if overlap and previous[-overlap:] != current[:overlap]:
                overlap = next(
                    (count for count in range(overlap + 1, len(current) + 1)
                     if previous[-count:] == current[:count]),
                    None,
                )
                assert overlap is not None, "native Review pages lost ordered overlap"
            # Equal text is not a scroll boundary: preserve repeated logical
            # body rows, including when small viewports genuinely do not overlap.
            merged.extend(current[overlap:])
        previous = current
    return merged


def _squash(text):
    return "".join(text.split())


def _card_rows(screen):
    # Only the product's double-bordered overlay, never the prose behind it.
    return [row.split("║", 2)[1].rstrip() for row in screen.splitlines() if row.count("║") >= 2]


def _assert_card(screen, expected_body, *, reviewing=False):
    rows = _card_rows(screen)
    joined = _squash("".join(rows))
    assert joined.startswith(_squash(HEADING)), "product proposal heading missing\n" + screen
    # Deliberately after the heading-only progress fence. Footer aliases and
    # body-spoof aliases cannot satisfy this independent dedicated block.
    before_type = joined.split(_squash(TYPE), 1)[0]
    assert before_type.removeprefix(_squash(HEADING)) == _squash(ALIASES), (
        "complete proposal alias block is missing or truncated\n" + screen
    )
    assert _squash(TYPE) in joined, "proposal type block is missing or truncated\n" + screen
    footer_start = next((i for i, row in enumerate(rows) if row.startswith("[y] Send to ")), None)
    assert footer_start is not None, "true proposal action row missing\n" + screen
    assert _squash("".join(rows[footer_start:])) == _squash(FOOTER), (
        "true footer must be the final complete action-row group\n" + screen
    )
    type_start = next(i for i, row in enumerate(rows) if row.startswith("Type: "))
    middle = rows[type_start + 1:footer_start]
    assert all(row.startswith("│ ") or row.startswith("… more not shown") for row in middle), (
        "proposal body/spoof chrome escaped the delimited body region\n" + screen
    )
    preview = [row[2:] for row in middle if row.startswith("│ ")]
    if reviewing:
        assert not preview, "Review overlay must not masquerade as the Info body\n" + screen
    else:
        raw_rows = [row.split("║", 2)[1] for row in screen.splitlines() if row.count("║") >= 2]
        body_width = len(raw_rows[0]) - 4
        expected = _painted_rows(expected_body, body_width)
        area_height = next(i for i, row in enumerate(screen.splitlines()) if row.startswith(" (daemon)"))
        chrome_rows = type_start + 1 + len(rows) - footer_start
        count = min(4, area_height - 3 - chrome_rows, len(expected))
        assert count > 0 and preview == expected[:count], (
            "native proposal preview is not the verbatim wrapped body prefix\n" + screen
        )
        assert any(row.startswith("… more not shown") for row in middle), (
            "uncapped body must disclose Review first\n" + screen
        )
    return rows


class _ObservedHost(_ObservedHttp):
    """Reuse the existing counter/forwarder; observe, never synthesize, wire text.

    TLS and bound peerKeys are necessary for actual marcus-arch attribution:
    product loopback listeners intentionally collapse callers and omit aliases.
    No credential, capability value or raw header is retained in the ledger.
    """

    def __init__(self, cert, key, tls_context):
        super().__init__(ssl_context=tls_context)
        self.ledger = []
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(cert, key)
        self.server.socket = context.wrap_socket(self.server.socket, server_side=True)
        owner = self
        original = self.server.RequestHandlerClass

        class Observed(original):
            def forward(self):
                row = {"method": self.command, "path": self.path}
                if self.command == "POST":
                    raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
                    # The inherited forwarder still consumes and forwards the
                    # original bytes. Only our local stream is replayed to it.
                    self.rfile = io.BytesIO(raw)
                    payload = json.loads(raw)
                    message = payload.get("params", {}).get("message", {})
                    metadata = message.get("metadata", {})
                    texts = [part["text"] for part in message.get("parts", []) if part.get("kind") == "text"]
                    row.update({
                        "rpc_method": payload.get("method"), "request_sha256": hashlib.sha256(raw).hexdigest(),
                        "message_id": message.get("messageId"), "text_parts": texts,
                        "text_sha256": [hashlib.sha256(text.encode()).hexdigest() for text in texts],
                        "text_bytes": [len(text.encode()) for text in texts],
                        "message_type": metadata.get("x-rustain-message-type"),
                        "metadata_keys": sorted(metadata),
                    })
                with owner.lock:
                    row["sequence"] = len(owner.ledger) + 1
                    owner.ledger.append(row)
                super().forward()

        self.server.RequestHandlerClass = Observed

    def requests(self):
        with self.lock:
            return [dict(row) for row in self.ledger]


def _tls_material(root):
    assert shutil.which("openssl"), "native attributed hosts require the installed openssl executable"
    ca, ca_key = root / "ca.pem", root / "ca.key"
    cert, key, csr = root / "server.pem", root / "server.key", root / "server.csr"
    extension = root / "server.ext"
    extension.write_text(
        "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\n"
        "extendedKeyUsage=serverAuth\nsubjectAltName=IP:127.0.0.1,DNS:localhost\n"
        "subjectKeyIdentifier=hash\nauthorityKeyIdentifier=keyid,issuer\n"
    )
    commands = [
        ["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
         "-addext", "keyUsage=critical,keyCertSign,cRLSign",
         "-subj", "/CN=Conflict capture ephemeral CA", "-keyout", str(ca_key), "-out", str(ca)],
        ["openssl", "req", "-new", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=localhost",
         "-keyout", str(key), "-out", str(csr)],
        ["openssl", "x509", "-req", "-in", str(csr), "-CA", str(ca), "-CAkey", str(ca_key),
         "-CAcreateserial", "-days", "1", "-extfile", str(extension), "-out", str(cert)],
    ]
    for command in commands:
        subprocess.run(command, check=True, capture_output=True, text=True, timeout=30)
    return ca, cert, key


@pytest.mark.parametrize("columns,rows", [(80, 24), (130, 30), (60, 16)], ids=["80x24", "130x30", "60x16"])
@pytest.mark.timeout(420)
def test_conflict_flag_capture(columns, rows, monkeypatch, conflict_run):
    binary_name = os.environ.get("RUSTAIN_TUI_BINARY")
    assert binary_name, "conflict capture requires an explicitly built four-feature release binary"
    binary = Path(binary_name).resolve()
    assert binary.is_file(), binary
    assert not os.environ.get("RUSTAIN_TUI_CLIENT_BINARY"), "capture uses the same explicit release for hosts and clients"
    binary_sha, source_sha, scene_sha = _sha(binary), _sha(SOURCE), _sha(SCENE)
    assert binary.stat().st_size <= 39_845_888, "four-feature release exceeds the approved byte ceiling"
    stamp_time = datetime.now(timezone.utc).replace(microsecond=0)
    stamp = stamp_time.strftime("%Y-%m-%dT%H-%M-%SZ")
    recorded = stamp_time.strftime("%Y-%m-%dT%H:%M:%SZ")
    identity = f"19-21b-{columns}x{rows}-{stamp_time.strftime('%Y%m%dT%H%M%SZ')}-{uuid.uuid4().hex}"
    receipt_dir = RECEIPTS / identity
    receipt_dir.mkdir(parents=True, exist_ok=False)
    extra = os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR")
    export_dir = Path(extra).resolve() / identity if extra else None
    root = Path(tempfile.mkdtemp(dir="/tmp", prefix="r21h-"))
    monkeypatch.setenv("RUSTAIN_TUI_BINARY", str(binary))
    monkeypatch.setattr(harness, "TERM_COLS", columns)
    monkeypatch.setattr(harness, "TERM_ROWS", rows)
    snapshots, checkpoints, hosts = [], [], {}
    ca, cert, key = _tls_material(root)
    tls_context = ssl.create_default_context(cafile=str(ca))
    workspaces = {alias: root / alias for alias in PERSONAS}
    homes = {alias: root / (alias + "-home") for alias in PERSONAS}
    assert len(str(root / "marcus-arch/.rustain_data/daemons" / ("a" * 16 + ".sock"))) <= 100
    manifest = {
        "recorded_at": recorded, "run": identity, "columns": columns, "rows": rows,
        "daemon_binary": str(binary), "daemon_binary_sha256": binary_sha,
        "daemon_binary_bytes": binary.stat().st_size,
        "client_binary": str(binary), "client_binary_sha256": binary_sha,
        "client_binary_bytes": binary.stat().st_size,
        "source_sha256": source_sha, "scene_sha256": scene_sha,
        "tls": {"ca_sha256": _sha(ca), "certificate_sha256": _sha(cert),
                "listener_bind": "0.0.0.0", "advertised_host": "127.0.0.1",
                "scope": "ephemeral local test hosts; synthetic operator-bound peer keys; no external peer"},
        "limits": "Exact native conflict sequence only; no recipient authorship carrier, authenticated-journal or J10 closure claim.",
    }

    def save(tui, name):
        screen = tui.get_screen_text()
        snapshots.append((name, screen))
        return screen

    def retain():
        # This callback runs after PTYs, daemons and observer threads stop.
        # Retain failures as well, without ever minting a success transcript.
        for name, screen in snapshots:
            (receipt_dir / (name + ".screen.txt")).write_text(screen)
        for path in root.rglob("*"):
            if path.is_file() and (
                path.name == "requests.jsonl"
                or ".runtime.log" in path.name
                or path.name.endswith((".daemon.log", ".runtime.log", ".terminal.txt", ".team-log.json"))
                or path.suffix == ".jsonl" and ".rustain/rooms" in path.as_posix()
            ):
                target = receipt_dir / path.relative_to(root)
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(path, target)
        (receipt_dir / "identity.json").write_text(json.dumps(manifest, indent=2) + "\n")
        (receipt_dir / "checkpoints.json").write_text(json.dumps(checkpoints, indent=2) + "\n")
        for alias, host in hosts.items():
            (receipt_dir / (alias + ".http.json")).write_text(json.dumps(host.requests(), indent=2) + "\n")
        if export_dir is not None:
            assert export_dir != receipt_dir.resolve(), "extra evidence copy must not overwrite the genuine run"
            shutil.copytree(receipt_dir, export_dir, dirs_exist_ok=False)
        shutil.rmtree(root)

    with ExitStack() as resources:
        resources.callback(retain)
        stub = resources.enter_context(SceneStub(SCENE, root / "requests.jsonl"))
        ports, reservations, environments, daemons = {}, {}, {}, {}
        for alias, workspace in workspaces.items():
            workspace.mkdir()
            homes[alias].mkdir()
            (workspace / ".env").write_text("")
            host = _ObservedHost(cert, key, tls_context)
            hosts[alias] = host
            resources.callback(host.close)
            reservation = socket.socket()
            reservation.bind(("0.0.0.0", 0))
            resources.callback(reservation.close)
            reservations[alias] = reservation
            ports[alias] = reservation.getsockname()[1]
            host.backend = f"https://127.0.0.1:{ports[alias]}"
        endpoints = {alias: f"https://127.0.0.1:{host.port}/" for alias, host in hosts.items()}
        # Provision real ephemeral Ed25519 fixture identities in the product's
        # existing key-store format. Their public keys pin the genuinely signed
        # AgentCards; bound peerKeys assert possession, not a human identity.
        pins = {}
        for alias, workspace in workspaces.items():
            data = workspace / ".rustain_data"
            data.mkdir()
            identity_key = data / "peer-identity.pkcs8.der"
            subprocess.run(["openssl", "genpkey", "-algorithm", "ED25519", "-outform", "DER",
                            "-out", str(identity_key)], check=True, capture_output=True, timeout=30)
            identity_key.chmod(0o600)
            public = subprocess.run(["openssl", "pkey", "-inform", "DER", "-in", str(identity_key),
                                     "-pubout", "-outform", "DER"], check=True, capture_output=True, timeout=30).stdout
            assert public.startswith(bytes.fromhex("302a300506032b6570032100")) and len(public) == 44, "unexpected Ed25519 public-key encoding"
            pins[alias] = base64.urlsafe_b64encode(public[-32:]).decode().rstrip("=")
        manifest["fixture_identity_public_keys"] = pins
        key_names = {alias: "CONFLICT_" + alias.upper().replace("-", "_") + "_KEY" for alias in PERSONAS}
        credentials = {name: "capture-" + uuid.uuid4().hex for name in key_names.values()}
        for alias, workspace in workspaces.items():
            config = workspace / ".rustain"
            config.mkdir()
            (config / "a2a.json").write_text(json.dumps({
                "server": {"admission": "allow", "advertisedHost": f"127.0.0.1:{hosts[alias].port}",
                           "tls": {"cert": str(cert), "key": str(key)}, "threadReads": "credential-scoped",
                           "peerKeys": {key_names[peer]: peer for peer in PERSONAS if peer != alias},
                           "requirePeerBinding": True},
                "agents": {peer: {"url": endpoints[peer], "auth": key_names[alias], "caCert": str(ca),
                                  "pinnedKey": {"alg": "EdDSA", "x": pins[peer]}}
                           for peer in PERSONAS if peer != alias},
            }))
            (config / "a2a-interaction.toml").write_text(
                '[interaction.defaults]\nresponse_mode = "notify-and-wait"\nnotification = "immediate"\n')
            (config / "config.toml").write_text('[permissions]\nalways_tools = ["Read"]\n')
            environments[alias] = {
                **os.environ, **stub.env(PERSONAS[alias], homes[alias]), **credentials,
                "TERM": "xterm-256color", "RUST_LOG": "debug", "RUSTAIN_CONFIG_DIR": str(config),
                "RUSTAIN_DATA_DIR": str(workspace / ".rustain_data"),
                "RUSTAIN_LOG_PATH": str(root / (alias + ".runtime.log")),
            }

        def start_daemon(alias, generation=1):
            log_path = root / f"{alias}-{generation}.daemon.log"
            log = resources.enter_context(log_path.open("w"))
            reservation = reservations.pop(alias, None)
            if reservation is not None:
                reservation.close()
            process = subprocess.Popen(
                [str(binary), f"--serve-a2a=0.0.0.0:{ports[alias]}", "daemon", "start", "--foreground"],
                cwd=workspaces[alias], env=environments[alias], stdout=log,
                stderr=subprocess.STDOUT, start_new_session=True,
            )
            resources.callback(_stop, process)
            daemons[alias] = process

            last_probe_error = ""
            def ready():
                nonlocal last_probe_error
                assert process.poll() is None, log_path.read_text()
                try:
                    with urllib.request.urlopen(hosts[alias].backend + "/.well-known/agent-card.json",
                                                context=tls_context, timeout=1) as reply:
                        return reply.status == 200
                except (OSError, urllib.error.URLError) as error:
                    last_probe_error = f"{type(error).__name__}: {error}"
                    return False
            _wait(ready, lambda: "\n".join([
                log_path.read_text(), last_probe_error,
                *(path.read_text() for path in root.glob(alias + ".runtime.log*")),
            ]))

        def catalog_ready(alias, peers):
            def observed():
                text = "\n".join(path.read_text() for path in root.glob(alias + ".runtime.log*"))
                return text if all("A2aCatalogChanged" in text and f"peer_id={peer}" in text for peer in peers) else None
            return _wait(observed, lambda: "\n".join(path.read_text() for path in root.glob(alias + ".runtime.log*")))

        # Lena's first ingress lets Marcus discover her; restart Lena before
        # attachment so her final outbound runtime discovers the now-live Marcus.
        # Jun only receives. Never restart a daemon after a proposal is created.
        for alias in ("lena-po", "jun-dev", "marcus-arch"):
            start_daemon(alias)
        catalog_ready("marcus-arch", RECIPIENTS)
        _stop(daemons["lena-po"])
        start_daemon("lena-po", generation=2)
        manifest["boot_order"] = ["lena-po", "jun-dev", "marcus-arch", "restart:lena-po"]

        catalog_ready("lena-po", ("marcus-arch",))

        def start_tui(alias):
            tui = resources.enter_context(RustainTUI(
                fresh=False, build=False, binary=binary, workspace=workspaces[alias], allowed_tools=["Read"],
                extra_args=["daemon", "attach"], env_overrides=environments[alias], timeout=30,
            ))
            tui.child.logfile_read = resources.enter_context((root / (alias + ".terminal.txt")).open("w"))
            assert tui.wait_for_screen("Ready", timeout=30), tui.get_screen_text()
            save(tui, alias + "-attached-ready")
            return tui

        lena, jun, marcus = (start_tui(alias) for alias in PERSONAS)
        tuis = {"lena-po": lena, "jun-dev": jun, "marcus-arch": marcus}

        def turns(alias):
            return [row for row in stub.rows() if row.get("persona") == PERSONAS[alias] and "turn" in row]

        def checkpoint(name):
            result = {
                "name": name, "http": {alias: host.snapshot() for alias, host in hosts.items()},
                "events": {alias: {event: _events(workspace, event) for event in EVENT_NAMES}
                           for alias, workspace in workspaces.items()},
                "provider_turns": {alias: len(turns(alias)) for alias in PERSONAS},
            }
            checkpoints.append(result)
            return result

        def zero_effects(before, name):
            after = checkpoint(name)
            assert after["http"] == before["http"], f"{name}: proposal/Review/Dismiss contacted a peer: {before['http']} -> {after['http']}"
            assert after["events"] == before["events"], f"{name}: proposal/Review/Dismiss mutated action, dispatch or recipient journals"
            after["zero_effect_from"] = before["name"]
            return after

        def no_model_turns(name):
            assert all(not turns(alias) for alias in PERSONAS), f"{name}: arrival/View/Acknowledge/context triggered a model proposal: {stub.rows()}"

        warmup = checkpoint("boot-discovery-and-attachment-complete")
        no_model_turns("warmup")
        _command(lena, "/team send --type story_assignment --subject adr-047 marcus-arch " + STORY)
        assert marcus.wait_for_screen("[y] Allow once", timeout=45), marcus.get_screen_text()
        marcus.send("y")
        assert marcus.wait_for_screen("story_assignment · lena-po", timeout=45), marcus.get_screen_text()
        save(marcus, "marcus-real-story-arrival")
        no_model_turns("arrival")
        item = _painted_item(marcus, "marcus-assignment", snapshots)
        viewed = _surface_pages(marcus, "marcus-assignment-view", snapshots)
        painted_story, cursor = _squash(_compact(viewed)), 0
        for line in STORY.splitlines():
            needle = _squash(line)
            position = painted_story.find(needle, cursor)
            assert position >= cursor, "ordered native assignment body is missing\n" + viewed
            cursor = position + len(needle)
        assert f"1 text body · {len(STORY.encode())} bytes" in viewed, viewed
        assert not _events(workspaces["marcus-arch"], "recipient_item_acknowledged"), "passive View acknowledged the assignment"
        no_model_turns("View")
        marcus.send("a")
        _wait(lambda: len(_events(workspaces["marcus-arch"], "recipient_item_acknowledged")) == 1, marcus.get_screen_text)
        no_model_turns("Acknowledge")
        marcus.send("c")
        assert marcus.wait_for_screen("[c] Stop using", timeout=15), marcus.get_screen_text()
        assert marcus.wait_for_screen("peer-tainted", timeout=15), marcus.get_screen_text()
        save(marcus, "marcus-explicit-peer-tainted-context")
        no_model_turns("context selection")
        marcus.send("\x1b")
        _wait(lambda: "item actions ·" not in marcus.get_screen_text(), marcus.get_screen_text)
        _wait(lambda: len(_events(workspaces["lena-po"], "team_send_recipient_settled")) == 1, lena.get_screen_text)
        arrival = _events(workspaces["marcus-arch"], "recipient_item_received")
        assert len(arrival) == 1 and arrival[0]["content"] == STORY, arrival
        assert arrival[0]["address"]["item"] == item and arrival[0]["alias"] == "lena-po", arrival
        before_proposal = checkpoint("distribution-settled-before-explicit-operator-turn")

        scene = json.loads(SCENE.read_text())
        scripted = scene["personas"][PERSONAS["marcus-arch"]]["turns"]
        body, spoof_body = (scripted[index]["content"][0]["input"]["body"] for index in (0, 2))
        body_sha256 = hashlib.sha256(body.encode()).hexdigest()
        # The product's content hash covers its canonical JSON string, not raw UTF-8.
        action_body_hash = hashlib.sha256(json.dumps(body, ensure_ascii=False).encode()).hexdigest()

        def proposal(prompt, expected_body, expected_turns, name):
            _command(marcus, prompt)
            screen = _wait(
                lambda: screen if _squash("".join(_card_rows(screen := marcus.get_screen_text()))).startswith(_squash(HEADING)) else None,
                marcus.get_screen_text,
            )
            snapshots.append((name + "-card", screen))
            _assert_card(screen, expected_body)
            # Continuation is a real provider request; no served prose fences
            # either card progress or the model turn's completion.
            _wait(lambda: len(turns("marcus-arch")) == expected_turns, lambda: stub.rows())
            assert marcus.wait_for_screen("Ready", timeout=30), marcus.get_screen_text()
            _assert_card(save(marcus, name + "-card-complete-turn"), expected_body)

        def review(expected_body, name):
            last_body_row = next(
                row for row in reversed(_painted_rows(expected_body, columns - 2)) if row.strip()
            )
            if name == "first":
                _command(marcus, f"/team view {item}")
                save(marcus, name + "-recipient-surface-before-review")
            else:
                marcus.send("r")
                _wait(
                    lambda: "│ " + last_body_row in unicodedata.normalize("NFC", marcus.get_screen_text()),
                    marcus.get_screen_text,
                )
                for _ in range(12):
                    marcus.send("\x1b[5~")
            marcus.send("r")
            _wait(
                lambda: "│ " + last_body_row in unicodedata.normalize("NFC", marcus.get_screen_text()),
                marcus.get_screen_text,
            )
            _wait_for_marker(marcus, "Proposed message (not sent)", marcus.get_screen_text)
            save(marcus, name + "-review-open")
            rendered = _chat_pages(marcus, name + "-review-full", snapshots)
            actual = _review_rows(rendered, rows)
            width = columns
            title = _painted_rows("Proposed message (not sent) · architecture_update", width)
            aliases = _painted_rows(ALIASES, width)
            expected = title + aliases + ["│ " + row for row in _painted_rows(expected_body, width - 2)]
            starts = [index for index, row in enumerate(actual) if row == title[0]]
            assert starts, "native Review Info title/type is missing\n" + rendered
            start = starts[-1]
            assert actual[start:start + len(title)] == title, "native Review Info title/type is incomplete\n" + rendered
            alias_start = start + len(title)
            assert actual[alias_start:alias_start + len(aliases)] == aliases, (
                "complete native Review dedicated alias block is missing or truncated\n" + rendered
            )
            assert actual[start:start + len(expected)] == expected, (
                "full native Review body is missing, reordered or not verbatim delimited\n" + rendered
            )
            end = start + len(expected)
            assert end == len(actual) or not actual[end].startswith("│ "), (
                "native Review body has unexpected extra delimited rows\n" + rendered
            )
            _assert_card(save(marcus, name + "-review-footer"), expected_body, reviewing=True)
            return rendered

        proposal("Check this selected story assignment against ADR-047 and propose the exact finding to Lena and Jun; never send it.", body, 2, "first")
        zero_effects(before_proposal, "first-card-before-any-decision")
        first_turn = turns("marcus-arch")[0]
        assert "propose_team_message" in first_turn["tool_names"], first_turn
        assert first_turn["tool_results"] == 0, first_turn
        assert first_turn["request_observations"]["needle_present"] == {"selected_assignment": True, "peer_data_boundary": True}, first_turn
        review(body, "first")
        zero_effects(before_proposal, "first-full-review-before-human-send")
        marcus.send("y")
        for alias in RECIPIENTS:
            tui = tuis[alias]
            assert tui.wait_for_screen("[y] Allow once", timeout=45), tui.get_screen_text()
            tui.send("y")
            assert tui.wait_for_screen("architecture_update · marcus-arch", timeout=45), tui.get_screen_text()
            save(tui, alias + "-actual-architecture-update-card")
        _wait(lambda: len(_events(workspaces["marcus-arch"], "team_send_recipient_settled")) == 2, marcus.get_screen_text)
        # As in Act 1, a separate product-local status response fences the
        # feedback consumer before inspecting the entire original send block.
        _command(marcus, "/team status")
        _wait_for_marker(marcus, "- jun-dev", marcus.get_screen_text)
        sender_feedback = _chat_pages(marcus, "marcus-entire-settled-send-block", snapshots)
        compact_feedback = _compact(sender_feedback)
        expected_rows = ("lena-po awaiting their approval", "jun-dev awaiting their approval")
        positions = [compact_feedback.find(row) for row in expected_rows]
        assert all(position >= 0 for position in positions) and positions == sorted(positions), (
            "Marcus entire settled sender block must show both ordered recipient outcomes\n" + sender_feedback
        )
        assert "sending…" not in compact_feedback, "Marcus sender block did not settle\n" + sender_feedback
        after_send = checkpoint("human-send-settled-before-board-or-log-traffic")
        for alias in RECIPIENTS:
            assert after_send["http"][alias]["get"] == before_proposal["http"][alias]["get"], "Send unexpectedly repeated discovery"
            assert after_send["http"][alias]["post"] - before_proposal["http"][alias]["post"] == 1, "human Send must make exactly one canonical recipient POST"
            posts = [request for request in hosts[alias].requests() if request.get("rpc_method") in ("message/send", "tasks/send")]
            assert len(posts) == 1, posts
            assert posts[0]["text_parts"] == [body] and posts[0]["message_type"] == "architecture_update", posts
            assert posts[0]["text_sha256"] == [body_sha256], posts
            assert posts[0]["text_bytes"] == [len(body.encode())], posts
            assert not any("author" in key for key in posts[0]["metadata_keys"]), "recipient authorship carrier is outside this story"
            received = _events(workspaces[alias], "recipient_item_received")
            assert len(received) == 1 and received[0]["content"] == body, received
            assert received[0]["alias"] == "marcus-arch" and received[0]["message_type"] == "architecture_update", received
            assert received[0]["task"] == posts[0]["message_id"], (received, posts)
        actions = _events(workspaces["marcus-arch"], "team_send_action_recorded")
        dispatches = _events(workspaces["marcus-arch"], "remote_envelope_dispatched")
        assert len(actions) == 1 and len(dispatches) == 2, (actions, dispatches)
        action = actions[0]
        assert [recipient["alias"] for recipient in action["recipients"]] == list(RECIPIENTS), action
        assert action["message_type"] == "architecture_update", action
        assert action["body_bytes"] == len(body.encode()) and action["body_hash"] == action_body_hash, action
        assert {row["action"]["index"] for row in dispatches} == {0, 1}, dispatches
        for dispatch in dispatches:
            assert dispatch["action"]["action"] == action["action"] and dispatch["authorship"] == "agent-composed", dispatch
            assert dispatch["content_hash"] == action["body_hash"], dispatch

        # A first selector has no prior board to masquerade as freshness. Prove
        # the exact correlated action and real read traffic after the zero-I/O
        # interval, rather than counting legitimate board POSTs as sends.
        board = _fresh_board(marcus, "/team board --last", "Distribution board · architecture_update",
                             "marcus-fresh-last-action", snapshots)
        board_compact = _compact(board)
        assert "2 addressed" in board_compact, board
        for alias in RECIPIENTS:
            assert "● " + alias in board_compact, board
        _command(marcus, "/team log")
        _wait_for_marker(marcus, "[agent-composed]", marcus.get_screen_text)
        product_log_screen = _chat_pages(marcus, "marcus-agent-composed-log", snapshots)
        assert "[agent-composed]" in product_log_screen and "[auto-sent]" not in product_log_screen, product_log_screen
        after_reads = checkpoint("legitimate-board-and-log-traffic-complete")
        for alias in RECIPIENTS:
            assert after_reads["http"][alias]["post"] > after_send["http"][alias]["post"], "fresh board did not read its addressed peer"
            assert len([request for request in hosts[alias].requests() if request.get("rpc_method") == "message/send"]) == 1

        before_dismiss = checkpoint("second-proposal-after-read-traffic-baseline")
        proposal("Prepare a second architecture-check draft quoting the supplied spoof chrome as body data; I will dismiss it.", spoof_body, 4, "second-spoof")
        zero_effects(before_dismiss, "second-spoof-card-before-decision")
        second_card = snapshots[-1][1]
        for spoof in ("To: spoof-recipient", "[y] Send to spoof-recipient"):
            for row in _card_rows(second_card):
                if spoof in row:
                    assert row.startswith("│ "), "spoof chrome escaped the card body\n" + second_card
        review(spoof_body, "second-spoof")
        zero_effects(before_dismiss, "second-spoof-full-review-before-dismiss")
        marcus.send("n")
        _wait_for_marker(marcus, "Proposal dismissed. Nothing was sent.", marcus.get_screen_text)
        dismissed = _chat_pages(marcus, "second-native-dismiss-T8", snapshots)
        assert "Proposal dismissed. Nothing was sent." in _compact(dismissed), dismissed
        assert not _card_rows(marcus.get_screen_text()), "dismissed proposal remained painted"
        zero_effects(before_dismiss, "second-native-dismiss-zero-effects")
        assert [row["turn"] for row in turns("marcus-arch")] == [1, 2, 3, 4], stub.rows()
        assert all(not turns(alias) for alias in RECIPIENTS), stub.rows()
        assert not any(row.get("desync") or row.get("kind") in ("desync", "unknown") for row in stub.rows()), stub.rows()
        assert all(row["tool_results"] == 1 for row in turns("marcus-arch")[1::2]), stub.rows()

        logs = {}
        for alias, workspace in workspaces.items():
            result = subprocess.run([str(binary), "team", "log", "--json"], cwd=workspace,
                                    env=environments[alias], check=True, capture_output=True, text=True, timeout=30)
            parsed = json.loads(result.stdout)
            assert parsed["rows"], result.stdout
            logs[alias] = result.stdout
            (root / (alias + ".team-log.json")).write_text(result.stdout)
        assert _sha(binary) == binary_sha and _sha(SOURCE) == source_sha and _sha(SCENE) == scene_sha, "capture input identity changed during native run"
        manifest.update({
            "assignment_item_from_painted_surface": item,
            "body_sha256_from_actual_posts_and_journals": body_sha256,
            "sender_action_json_text_sha256": action["body_hash"],
            "body_bytes_from_actual_posts_and_journals": action["body_bytes"], "send_action": action["action"],
            "provider_turns": {alias: len(turns(alias)) for alias in PERSONAS},
            "scene_request_ledger": "requests.jsonl", "snapshots": [name for name, _ in snapshots],
            "sender_feedback": list(expected_rows), "sender_feedback_asserted_before_first_board": True,
            "zero_effect_intervals": [{"from": point["zero_effect_from"], "to": point["name"]}
                                      for point in checkpoints if "zero_effect_from" in point],
            "boot_http_checkpoint": warmup["http"],
        })
        receipt = receipt_dir / f"story-19-21b-conflict-flag-{stamp}.transcript.txt"
        staged = receipt_dir / f"story-19-21b-conflict-flag-{stamp}.staged"
        with staged.open("x") as stream:
            sections = [
                f"recorded-at: {recorded}\n"
                f"scene: tests_tui/journeys/j10/scenes/19-21b-conflict-flag-v1.json sha256: {scene_sha}\n"
                f"source: tests_tui/test_story_19_21b_conflict_flag.py sha256: {source_sha}\n",
                json.dumps(manifest, indent=2),
            ]
            for alias, text in logs.items():
                sections.append(f"\n--- actual {alias} product team log --json ---\n{text}")
            sections.append("\n--- actual HTTP request observations (allowlisted) ---\n" + json.dumps(
                {alias: host.requests() for alias, host in hosts.items()}, indent=2))
            sections.append("\n--- actual scene request ledger ---\n" + json.dumps(stub.rows(), indent=2))
            sections.append("\n--- zero-effect intervals and legitimate-traffic checkpoints ---\n" + json.dumps(checkpoints, indent=2))
            for name, screen in snapshots:
                sections.append(f"\n--- actual painted {name} ({columns}x{rows}) ---\n{screen}")
            stream.write("\n".join(sections))
    receipt = conflict_run.publish_receipt(
        f"{columns}x{rows}", staged, receipt,
        exported_staged_path=export_dir / staged.name if export_dir is not None else None,
    )
    print(f"CONFLICT_FLAG_RECEIPT={receipt}", flush=True)
