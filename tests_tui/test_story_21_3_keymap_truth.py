"""Story 21.3 K7 (smoke leg A): ONE 80x24 native keymap smoke on a DEBUG binary.

Default-feature, non-API (no `requires_api`, no new marker): a loopback
Anthropic-shaped provider answers the startup probe and the model list, and any
real model request fails the test, so no credential or network is used. The
isolated config sets `[notifications] info_ttl_secs = 30` so a toast outlives
a loaded runner's slowest read; every toast is dismissed with `Ctrl+X x`.

The rows come from `tests_tui/fixtures/keymap_portable_rows.json` (generated
from the keymap registry by `keymap::render_docs::portable_rows_json`). Every
LOCAL leader row is sent as the row's own raw PTY bytes (`0x18` + follow-up,
written straight to the PTY master, never through a harness key helper) and
its surface is asserted on a SETTLED 80x24 screen. The fixture drives the loop,
but the surface each row must produce is declared HERE (`SURFACES`): a row the
test does not know, a row that vanished and a witness that drifted all fail,
so the screen text is never served by the fixture alone.

Settled means (the terminal delivers a frame in chunks and ratatui writes the
rows top to bottom):

* overlay - the title row, a continuous `│…│` body and the CLOSING border at
  the same columns (plus the footer painted on that border or the rail inside
  the box, where the overlay has one); no leader box on screen;
* toast - the whole painted line, right-aligned to the pane edge, directly
  above the status row;
* menu closed - no ` Ctrl+X ` box and the status tail back to `^X menu` (the
  status row is painted after every row above it).

The seven sidebar-panel rows (`a s l r e t h`) answer the same remedy toast and
a notice identical to one already raised is coalesced (a visible toast) or
silently counted (the 5 minute suppression window), so only the first of them
could ever paint. Each runs in its own fresh process.

Also asserted: `Ctrl+X j` -> `No binding for Ctrl+X j in Input`; the status
row ends with `^X menu`, with `^X…` while the leader is open and again with
`^X menu` after `Esc`; the local menu labels (`[E] Artifacts`, `[W] Monitor`,
`[L] A2A log`, `(n/a)` panels, no `[F]`/`[D]`/`Dashboard`).

Evidence (screens, raw PTY bytes sent, requests, runtime logs, binary hash)
goes to `RUSTAIN_CAPTURE_EVIDENCE_DIR`. Every wait is a bounded screen
condition.
"""
from contextlib import ExitStack
from datetime import datetime, timezone
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import queue
import re
import tempfile
import threading
import traceback
from urllib.parse import urlsplit

import harness
import pytest
from test_story_21_2_shortcut_truth import _NativeTUI, _capture, _menu, _menu_painted, _wait

KEY = "sk-ant-21-3-loopback-key-canary"
FIXTURE = Path(__file__).resolve().parent / "fixtures" / "keymap_portable_rows.json"
REGENERATE = "RUSTAIN_WRITE_KEYMAP_DOC=1 cargo test --test tui conformance_21_3_keymap::k6_docs"
CONFIG = (
    '[permissions]\nalways_tools = ["Read", "Write", "Edit", "Bash", "Glob", "Grep"]\n'
    "\n[notifications]\ninfo_ttl_secs = 30\n"
)
# `keymap/table.rs` PANEL_NARROW: the remedy a local sidebar-panel chord answers below 120 columns.
PANEL_REMEDY = "This panel needs a terminal at least 120 columns wide: widen the window and try again."
PANEL_WITNESS = "This panel needs a terminal at least 120"
PROBE_OK = (b'{"type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],'
            b'"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}')

# Leader follow-up bytes (`0x18` + this) -> the surface the row must produce at 80x24.
# overlay: (witness, title needle, text on the closing border or None, rail inside the box or None)
OVERLAYS = {
    "1870": ("Profile Switcher", "┌ Profile Switcher ", None, None),
    "186d": ("Select Model", "┌ Select Model ", None, None),
    "1869": ("Sign in", "┌ Sign in", None, None),
    "1875": ("Usage & Cost", "┌ Usage & Cost ", " Esc: close ", None),
    "183f": ("Keybindings", "┌ Help — Rustain Keybindings ", " j/k scroll  ? close ", None),
    "186e": ("Notifications", harness.NOTIFICATION_LOG_TITLE, None, "[Esc] Close"),
}
# panels: the narrow-terminal remedy toast; the value is the menu item painted `(n/a)` for the row.
PANELS = {
    "1861": "[A] Adapters (n/a)",
    "1873": "[S] Agents (n/a)",
    "186c": "[L] A2A log (n/a)",
    "1872": "[R] Room (n/a)",
    "1865": "[E] Artifacts (n/a)",
    "1874": "[T] Tasks (n/a)",
    "1868": "[H] History (n/a)",
}
DECISIONS = "1871"   # `Ctrl+X q`: Info toast, left visible for the `x` row to clear
CLEAR = "1878"       # `Ctrl+X x`
MONITOR = "1877"     # `Ctrl+X w`: density toggle, run LAST (it moves focus off the composer)
SILENT = ("181b", "1803", "180d")  # Esc, Ctrl+C, Enter (empty composer): close the menu, say nothing
EXPECTED_WITNESS = {
    **{hex_: spec[0] for hex_, spec in OVERLAYS.items()},
    **{hex_: PANEL_WITNESS for hex_ in PANELS},
    DECISIONS: "No decisions waiting",
    MONITOR: "Density: Monitor",
    CLEAR: "", "181b": "", "1803": "", "180d": "",
}
MENU_PRESENT = (
    "[P] Profile", "[M] Model", "[I] Sign in", "[A] Adapters (n/a)", "[S] Agents (n/a)",
    "[L] A2A log (n/a)", "[R] Room (n/a)", "[E] Artifacts (n/a)", "[T] Tasks (n/a)", "[U] Usage",
    "[H] History (n/a)", "[W] Monitor", "[?] Help", "[N] Notifications", "[X] Clear toasts",
    "[Q] Decisions", "[Enter] Send", "[Esc] Cancel",
)
MENU_ABSENT = ("[F]", "[D]", "Dashboard", "Evidence", "Watch", "[L] Log", "Detach",
               "[U] Usage (n/a)", "[W] Monitor (n/a)", "[P] Profile (n/a)", "[M] Model (n/a)")


def _fixture_rows():
    """Every LOCAL leader row of the generated fixture, keyed by its PTY bytes."""
    if not FIXTURE.is_file():
        pytest.fail(f"{FIXTURE} is missing; regenerate it with: {REGENERATE}")
    rows = json.loads(FIXTURE.read_text())["rows"]
    local = [row for row in rows if row["context"] == "Leader" and "local" in row["host"]]
    by_hex = {row["pty_hex"]: row for row in local}
    assert len(by_hex) == len(local), f"duplicate leader PTY bytes in {FIXTURE}"
    assert all(row["pty_hex"].startswith("18") and len(row["pty_hex"]) >= 4 for row in local), local
    known = set(EXPECTED_WITNESS)
    assert set(by_hex) == known, (
        f"fixture leader rows differ from this smoke's declared rows; regenerate with: {REGENERATE}\n"
        f"fixture only: {sorted(set(by_hex) - known)}  smoke only: {sorted(known - set(by_hex))}"
    )
    for hex_, row in by_hex.items():
        assert row["witness"] == EXPECTED_WITNESS[hex_], (hex_, row)
    return by_hex


# ── screen predicates (positional, anchored; nothing here is a bare substring search) ──

def _tail(screen):
    """The status row (directly above the composer), right-stripped: only its tail is asserted,
    the left segments clip by design at 80 columns."""
    return harness.status_row(screen).rstrip()


def _closed(screen):
    """The leader menu is gone and the status tail is back to `^X menu`."""
    return not _menu(screen) and _tail(screen).endswith("^X menu")


def _toasts(screen):
    """Toast rows above the status row: right-aligned, severity word first."""
    found = []
    for row in harness.toast_rows(screen, 3):
        text = row.rstrip()
        if re.search(r"(?<![A-Za-z0-9])(Info|Success|Warning|Error) \S", text) and len(text) >= 78:
            found.append(text)
    return found


def _remedy_toast(screen):
    """The panel remedy toast: `Info <remedy>`, cut by the 80-column row with `…`."""
    for text in _toasts(screen):
        match = re.search(r"(?<![A-Za-z0-9])Info (This panel needs a terminal at least 120.*)$", text)
        if match:
            body = match.group(1)
            if body == PANEL_REMEDY or (body.endswith("…") and PANEL_REMEDY.startswith(body[:-1])):
                return text
    return None


def _box(screen, needle, footer=None, rail=None):
    """Interior rows of the overlay whose title row holds `needle` (which starts with `┌`),
    else None until it is COMPLETE: a continuous `│…│` body down to the closing `└…┘` border
    at the same columns, the footer on that border and the rail inside the box."""
    lines = screen.split("\n")
    for top, line in enumerate(lines):
        left = line.find(needle)
        right = line.rfind("┐")
        if left < 0 or right <= left:
            continue
        for bottom in range(top + 1, len(lines)):
            row = lines[bottom]
            if len(row) <= right:
                return None
            if row[left] == "└" and row[right] == "┘":
                if footer is not None and footer not in row:
                    return None
                body = lines[top + 1:bottom]
                if rail is not None and rail not in " ".join(" ".join(body).split()):
                    return None
                return body
            if row[left] != "│" or row[right] != "│":
                return None
        return None
    return None


# ── driving ──────────────────────────────────────────────────────────────────

def _send(tui, sent, label, data):
    """Raw bytes straight to the PTY master (no harness key helper, no encoding step)."""
    sent.append({"label": label, "hex": data.hex()})
    os.write(tui.child.child_fd, data)


def _open_leader(tui, sent, label):
    """`0x18`: the untimed menu is fully painted (closing border + last item) and the
    status tail reads `^X…`. The status row is below the menu, so painted after it."""
    _send(tui, sent, label, b"\x18")
    return _wait(tui, lambda s: _menu_painted(s) and _tail(s).endswith("^X…"),
                 f"{label}: leader menu painted with the pending `^X…` tail")


def _clear_toasts(tui, sent, label):
    _open_leader(tui, sent, label)
    _send(tui, sent, label, b"x")
    return _wait(tui, lambda s: _closed(s) and not _toasts(s), f"{label}: toasts cleared")


def _silent_row(tui, evidence, sent, hex_, label):
    _open_leader(tui, sent, label)
    _send(tui, sent, label, bytes.fromhex(hex_)[1:])
    screen = _wait(tui, _closed, f"{label}: menu closed, tail back to `^X menu`")
    assert not _toasts(screen), f"{label} must close the menu silently:\n{screen}"
    assert tui.child.isalive(), label
    _capture(tui, evidence, f"row-{hex_}", screen)


def _overlay_row(tui, evidence, sent, hex_, row, label):
    witness, needle, footer, rail = OVERLAYS[hex_]
    _open_leader(tui, sent, label)
    _send(tui, sent, label, bytes.fromhex(hex_)[1:])
    screen = _wait(tui, lambda s: _box(s, needle, footer, rail) is not None and not _menu(s),
                   f"{label}: {witness!r} overlay complete (title, body, closing border)")
    assert row["witness"] in screen, screen
    _capture(tui, evidence, f"row-{hex_}", screen)
    _send(tui, sent, f"{label} Esc", b"\x1b")
    _wait(tui, lambda s: needle not in s and _closed(s), f"{label}: overlay closed by Esc")


def _exercise(binary, evidence, root, monkeypatch, rows):
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    errors = queue.Queue()
    posts = []
    lock = threading.Lock()
    ws_main, ws_panels = root / "wm", root / "wp"
    for ws in (ws_main, ws_panels):
        (ws / ".rustain").mkdir(parents=True)
        (ws / ".rustain" / "config.toml").write_text(CONFIG)
        (ws / ".env").write_text("")  # keep the shared harness from copying a developer .env
    (root / "data").mkdir()
    (root / "home").mkdir()

    def check_server():
        try:
            error = errors.get_nowait()
        except queue.Empty:
            return
        raise AssertionError(f"loopback handler failed:\n{error}")

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_args):
            pass

        def respond(self, content, status=200):
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(content)))
            self.end_headers()
            self.wfile.write(content)

        def do_GET(self):
            try:
                path = urlsplit(self.path).path
                self.respond(b"{}" if path == "/api.json" else b'{"data":[],"has_more":false}')
            except Exception:
                errors.put(traceback.format_exc())
                self.close_connection = True

        def do_POST(self):
            try:
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                with lock:
                    posts.append({"path": self.path, "max_tokens": body.get("max_tokens"), "stream": "stream" in body})
                if "stream" not in body and body.get("max_tokens") == 1:
                    self.respond(PROBE_OK)
                    return
                # A keymap smoke never sends a turn: a real model request is a failure.
                errors.put(f"unexpected provider request {self.path}: {json.dumps(body)[:300]}")
                self.respond(b"{}", status=500)
            except Exception:
                errors.put(traceback.format_exc())
                self.close_connection = True

    with ExitStack() as stack:
        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        server.daemon_threads = True
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()

        def close_server():
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)
            assert not thread.is_alive()

        stack.callback(close_server)
        origin = f"http://127.0.0.1:{server.server_port}"
        loaded = harness._load_env()
        cleared = {name for name in loaded if re.search(
            r"(?:API_KEY|AUTH_TOKEN|ACCESS_TOKEN|BASE_URL|ENDPOINT|PROXY|CREDENTIALS|PROJECT|PROJECT_ID)$", name, re.I)
            or name.startswith("RUSTAIN_")
            or name in {"OLLAMA_HOST", "OPENAI_CODEX_OAUTH_TOKEN"}}

        def environment(tag, ws):
            env = {name: "" for name in cleared}
            env.update(HOME=str(root / "home"), XDG_CONFIG_HOME=str(root / "home" / "xdg-config"),
                       XDG_DATA_HOME=str(root / "home" / "xdg-data"), XDG_CACHE_HOME=str(root / "home" / "xdg-cache"),
                       RUSTAIN_CONFIG_DIR=str(ws / ".rustain"), RUSTAIN_DATA_DIR=str(root / "data"),
                       RUSTAIN_LOG_PATH=str(evidence / f"{tag}.runtime.log"), RUST_LOG="debug",
                       ANTHROPIC_API_KEY=KEY, ANTHROPIC_AUTH_TOKEN="", ANTHROPIC_BASE_URL=origin,
                       RUSTAIN_MODELS_DEV_URL=origin, NO_PROXY="*", no_proxy="*", TERM="xterm-256color")
            return env

        envs = {"main": environment("main", ws_main), "panels": environment("panels", ws_panels)}
        terminals = {tag: stack.enter_context((evidence / f"{tag}.terminal.txt").open("w")) for tag in envs}
        screens = {tag: stack.enter_context((evidence / f"{tag}.screens.jsonl").open("w")) for tag in envs}
        original_spawn = harness.pexpect.spawn
        spawn_tag = {}

        def spawn(command, *args, **kwargs):
            tag = spawn_tag["next"]
            actual = kwargs["env"]
            assert Path(command).resolve() == binary and kwargs["dimensions"] == (24, 80)
            assert actual["ANTHROPIC_BASE_URL"] == origin and actual["ANTHROPIC_API_KEY"] == KEY
            kwargs["logfile"] = terminals[tag]
            return original_spawn(command, *args, **kwargs)

        monkeypatch.setattr(harness.pexpect, "spawn", spawn)

        def start(tag, ws):
            spawn_tag["next"] = tag
            tui = _NativeTUI(binary=binary, build=False, workspace=ws, env_overrides=envs[tag], fresh=True)
            tui.screens, tui.check_server = screens[tag], check_server
            try:
                tui.start()
            except BaseException:
                tui.stop()
                raise
            tui.child.delaybeforesend = 0
            return tui

        sent = []

        # ------------------------------------------------------------ main session
        tui = start("main", ws_main)
        stack.callback(tui.stop)

        # Status tail: `^X menu` at rest, `^X…` while the leader is open, `^X menu` after Esc.
        screen = _wait(tui, lambda s: _tail(s).endswith("^X menu"), "status row ends with `^X menu`")
        _capture(tui, evidence, "00-rest", screen)
        screen = _open_leader(tui, sent, "tail")
        menu = _menu(screen)
        for item in MENU_PRESENT:
            assert item in menu, (item, menu)
        for item in MENU_ABSENT:
            assert item not in menu, (item, menu)
        assert menu, screen
        _capture(tui, evidence, "01-leader-open", screen)
        _send(tui, sent, "tail Esc", b"\x1b")
        screen = _wait(tui, _closed, "Esc closes the menu; the tail returns to `^X menu`")
        _capture(tui, evidence, "02-leader-esc", screen)

        # A key with no row: one Info toast naming the key and the context, never silent.
        no_binding = "No binding for Ctrl+X j in Input"
        _open_leader(tui, sent, "no-binding")
        _send(tui, sent, "no-binding j", b"j")
        screen = _wait(tui, lambda s: _closed(s) and harness.toast_present(s, no_binding, "Info", rows=3),
                       "no-binding toast, menu closed")
        assert harness.toast_present(screen, no_binding, "Info", rows=3), screen
        _capture(tui, evidence, "03-no-binding", screen)
        _clear_toasts(tui, sent, "clear no-binding")

        # Every local fixture row, as its raw bytes. Order: silent rows while the composer is
        # focused, the toast row and the `x` row that clears it, the overlays, the density
        # toggle last (it moves focus off the composer), the panel rows in fresh processes.
        for hex_ in SILENT:
            _silent_row(tui, evidence, sent, hex_, rows[hex_]["key"])
            assert tui.child.isalive(), hex_

        row = rows[DECISIONS]
        _open_leader(tui, sent, row["key"])
        _send(tui, sent, row["key"], bytes.fromhex(DECISIONS)[1:])
        screen = _wait(tui, lambda s: _closed(s) and harness.toast_present(s, row["witness"], "Info", rows=3),
                       f"{row['key']}: {row['witness']!r} toast, menu closed")
        _capture(tui, evidence, f"row-{DECISIONS}", screen)

        row = rows[CLEAR]
        _open_leader(tui, sent, row["key"])
        _send(tui, sent, row["key"], bytes.fromhex(CLEAR)[1:])
        screen = _wait(tui, lambda s: _closed(s) and not _toasts(s), f"{row['key']}: the visible toast is hidden")
        _capture(tui, evidence, f"row-{CLEAR}", screen)

        for hex_ in OVERLAYS:
            _overlay_row(tui, evidence, sent, hex_, rows[hex_], rows[hex_]["key"])

        row = rows[MONITOR]
        _open_leader(tui, sent, row["key"])
        _send(tui, sent, row["key"], bytes.fromhex(MONITOR)[1:])
        screen = _wait(tui, lambda s: not _menu(s) and harness.toast_present(s, row["witness"], "Info", rows=3),
                       f"{row['key']}: {row['witness']!r} toast, menu closed")
        _capture(tui, evidence, f"row-{MONITOR}", screen)
        # The same chord toggles back (Monitor -> Focus).
        _open_leader(tui, sent, f"{row['key']} again")
        _send(tui, sent, f"{row['key']} again", bytes.fromhex(MONITOR)[1:])
        screen = _wait(tui, lambda s: _closed(s) and harness.toast_present(s, "Density: Focus", "Info", rows=3),
                       "Density: Focus toast, menu closed")
        _capture(tui, evidence, f"row-{MONITOR}-back", screen)
        assert tui.child.isalive()
        tui.stop()

        # ------------------------------------------------------ panel rows: one process each
        for hex_, item in PANELS.items():
            row = rows[hex_]
            panel = start("panels", ws_panels)
            try:
                screen = _open_leader(panel, sent, row["key"])
                assert item in _menu(screen), (item, _menu(screen))
                _send(panel, sent, row["key"], bytes.fromhex(hex_)[1:])
                screen = _wait(panel, lambda s: _closed(s) and _remedy_toast(s) is not None,
                               f"{row['key']}: the narrow-terminal remedy toast, menu closed")
                assert row["witness"] in screen, screen
                assert _remedy_toast(screen) is not None, screen
                _capture(panel, evidence, f"row-{hex_}", screen)
            finally:
                panel.stop()

        # ---------------------------------------------------------- evidence checks
        with lock:
            observed = list(posts)
        (evidence / "requests.json").write_text(json.dumps(observed, indent=2) + "\n")
        (evidence / "sent.json").write_text(json.dumps(sent, indent=2) + "\n")
        assert all(post["max_tokens"] == 1 and not post["stream"] for post in observed), observed
        check_server()
        for handle in (*terminals.values(), *screens.values()):
            handle.flush()
        logs = sorted(evidence.glob("*.runtime.log.*"))
        for tag in envs:
            assert any(path.name.startswith(f"{tag}.runtime.log.") and re.fullmatch(r"\d{4}-\d{2}-\d{2}", path.suffix[1:])
                       for path in logs), f"dated {tag} debug log missing"
        for path in [*(evidence / f"{tag}.terminal.txt" for tag in envs),
                     *(evidence / f"{tag}.screens.jsonl" for tag in envs),
                     evidence / "requests.json", evidence / "sent.json", *logs, *evidence.glob("*.screen.txt")]:
            assert KEY not in path.read_text(errors="replace"), f"credential leaked to {path}"


@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(480)
def test_keymap_truth_leader_rows_local_80x24(monkeypatch):
    rows = _fixture_rows()
    binary = harness._resolve_binary()[0].resolve()
    assert binary.is_file() and "debug" in binary.parts and "release" not in binary.parts, binary
    parent = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())).resolve()
    parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="21-3-", dir=parent))
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    manifest = {"binary": str(binary), "sha256": digest, "bytes": binary.stat().st_size, "viewport": [80, 24],
                "fixture": str(FIXTURE), "started_at": datetime.now(timezone.utc).isoformat(), "result": "RUNNING"}
    try:
        with tempfile.TemporaryDirectory(prefix="r213-") as root:
            _exercise(binary, evidence, Path(root), monkeypatch, rows)
        assert hashlib.sha256(binary.read_bytes()).hexdigest() == digest
        manifest["result"] = "PASS"
    except BaseException:
        manifest["result"] = "FAIL"
        (evidence / "failure.txt").write_text(traceback.format_exc())
        raise
    finally:
        manifest["completed_at"] = datetime.now(timezone.utc).isoformat()
        (evidence / "run.json").write_text(json.dumps(manifest, indent=2) + "\n")
        print(f"NATIVE_21_3_EVIDENCE={evidence}", flush=True)
