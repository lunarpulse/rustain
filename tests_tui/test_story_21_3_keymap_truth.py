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

Leg B (slice 3-B1, AC14): in a fresh process the six readline keys `0x01 0x05 0x17 0x15 0x0b 0x08`
(Ctrl+A/E/W/U/K/H) are sent as raw bytes to the composer and each is asserted by the composer text it
produces (the cursor and the kills move by grapheme: a combining accent goes with its letter). Then a
turn is held mid-stream by the loopback provider, `Ctrl+X ?` opens Help and a raw `Ctrl+C` must close
Help only: the stream is not interrupted and finishes (`Ready`, no `[interrupted]`). The readline rows
carry no smoke witness in the portable-row fixture (only leader rows do), so the six bytes are declared here.
The leg ends with the leader `Ctrl+X Ctrl+Q` row (fixture row `1811`): the process exits 0.

Leg C (slice 3-C, AC19): in a fresh process a model-proposed plan card is pending.
Arrival leaves Input focus unchanged; `y` is composer text (the card is not answered);
`Esc` moves to Chat, where a second `Esc` posts the registry-generated
cue and decides nothing, a raw `Ctrl+C` neither interrupts nor quits, and the explicit key `n` rejects the plan:
the provider sees exactly one rejection.

Leg D (3-C patch round, AC19, FR-21-3-c): a SEPARATE test (`test_keymap_truth_attached_permission_card_...`) drives the
permission card natively: a daemon plus a rich attached terminal at 80x24 and a scripted loopback provider that streams a
`Bash` tool call. `x` typed before the arrival is composer text; the approval arrives with an empty composer and focus
moves to Chat; the card paints `[y] Allow  [n] Deny`; `Esc` posts the cue (which names `[y] Allow  [n] Deny` inside the
clip) and the card stays pending; a raw `Ctrl+C` neither interrupts nor quits; `n` denies once (one tool result reaches
the provider, the command never ran).

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
import shlex
import shutil
import signal
import socket
import subprocess
import threading
import traceback
import time
import unicodedata
from urllib.parse import urlsplit

import harness
import pexpect
import pytest
from keys import LEFT, RIGHT
from test_story_20_9a_ephemeral_rows import CARD, _event, _finish, _plan_stream, _start_text
from test_story_21_2_shortcut_truth import _NativeTUI, _capture, _composer, _menu, _menu_painted, _wait
from test_story_19_20c_exact_approval import Evidence as _Evidence
from test_story_19_20c_exact_approval import _command, _finished, _world

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
QUIT = "1811"        # `Ctrl+X Ctrl+Q`: the process exits (leg B ends with it)
SILENT = ("181b", "1803", "180d")  # Esc, Ctrl+C, Enter (empty composer): close the menu, say nothing
EXPECTED_WITNESS = {
    **{hex_: spec[0] for hex_, spec in OVERLAYS.items()},
    **{hex_: PANEL_WITNESS for hex_ in PANELS},
    DECISIONS: "No decisions waiting",
    MONITOR: "Density: Monitor",
    CLEAR: "", "181b": "", "1803": "", "180d": "", QUIT: "",
}
MENU_PRESENT = (
    "[P] Profile", "[M] Model", "[I] Sign in", "[A] Adapters (n/a)", "[S] Agents (n/a)",
    "[L] A2A log (n/a)", "[R] Room (n/a)", "[E] Artifacts (n/a)", "[T] Tasks (n/a)", "[U] Usage",
    "[H] History (n/a)", "[W] Monitor", "[?] Help", "[N] Notifications", "[X] Clear toasts",
    "[Q] Decisions", "[Enter] Send", "[Esc] Cancel",
)
MENU_ABSENT = ("[F]", "[D]", "Dashboard", "Evidence", "Watch", "[L] Log", "Detach",
               "[U] Usage (n/a)", "[W] Monitor (n/a)", "[P] Profile (n/a)", "[M] Model (n/a)")

# Leg B (slice 3-B1). The six readline rows, as raw PTY bytes: Ctrl+A, Ctrl+E, Ctrl+W, Ctrl+U, Ctrl+K, Ctrl+H.
CTRL_A, CTRL_E, CTRL_W, CTRL_U, CTRL_K, CTRL_H = (bytes([code]) for code in (0x01, 0x05, 0x17, 0x15, 0x0B, 0x08))
HOLD_PROMPT = "hold-b please answer"
HELD_PARTIAL = "B-LIVE-PARTIAL"
HELD_TAIL = "B-TAIL-DONE"
HELP = "183f"  # `Ctrl+X ?`: the Help overlay
PLAN_PROMPT = "c propose the plan"


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


def _user_text(body):
    """Text of the last user message of a provider request body."""
    for message in reversed(body.get("messages", [])):
        if message.get("role") == "user":
            content = message.get("content")
            if isinstance(content, str):
                return content
            return " ".join(block.get("text", "") for block in content if isinstance(block, dict))
    return ""


def _chunk(data):
    return f"{len(data):x}\r\n".encode() + data + b"\r\n"


def _cue(screen):
    """The `Esc` / `Ctrl+C` cue toast of a decision card (A8); the card is named by its own words."""
    return any("never decide this card" in row for row in harness.toast_rows(screen, 3))


def _leg_c(tui, evidence, sent, rejections):
    """Leg C (3-C, AC16/AC17): a pending plan card is never answered by typing, `Esc` or `Ctrl+C`;
    the explicit key `n` rejects it once."""
    _send(tui, sent, "type plan prompt", PLAN_PROMPT.encode())
    _wait(tui, lambda s: _composer(s) == PLAN_PROMPT, "plan prompt typed")
    _send(tui, sent, "Enter", b"\r")
    screen = _wait(tui, lambda s: "Plan:" in s and CARD in s and "[y] Approve  [e] Edit  [n] Reject" in s
                   and "Ready" in harness.status_row(s), "the plan card (full generated rail) is pending and the turn is idle")
    _capture(tui, evidence, "c-01-plan-card", screen)
    assert not tui._screen.cursor.hidden, screen
    _send(tui, sent, "y in Input", b"y")
    screen = _wait(tui, lambda s: _composer(s) == "y", "`y` is composer text while the plan card is pending")
    assert "Plan:" in screen and CARD in screen, screen
    _send(tui, sent, "Ctrl+U", CTRL_U)
    _wait(tui, lambda s: _composer(s) == "", "composer cleared")
    # `Esc` in Input goes to Chat; there the card owns the keys and `Esc` is a cue, never a decision.
    _send(tui, sent, "Esc to Chat", b"\x1b")
    _wait(tui, lambda _s: tui._screen.cursor.hidden, "Chat focus")
    _send(tui, sent, "Esc on the card", b"\x1b")
    screen = _wait(tui, _cue, "Esc posts the cue naming the explicit keys")
    assert "Plan:" in screen and CARD in screen and tui._screen.cursor.hidden, screen
    _capture(tui, evidence, "c-02-esc-cue", screen)
    _send(tui, sent, "Ctrl+C on the card", b"\x03")
    screen = _wait(tui, lambda s: "Plan:" in s and CARD in s, "Ctrl+C leaves the plan card pending")
    assert tui.child.isalive() and "[interrupted]" not in screen and "System · Quit" not in screen, screen
    # Ctrl+C decided nothing: the card is still pending, so the explicit `n` below is what rejects it.
    _send(tui, sent, "n", b"n")
    screen = _wait(tui, lambda s: "rejected" in s.lower() and "Ready" in harness.status_row(s) and bool(rejections),
                   "the explicit key rejected the plan and the provider saw the rejection")
    _capture(tui, evidence, "c-03-plan-rejected", screen)


def _leg_b(tui, evidence, sent, release, turns, lock, row):
    """Leg B: raw readline bytes in the composer, then Ctrl+C in Help over a held stream."""
    def composer_is(expected, label):
        # NFC so a decomposed accent compares with its precomposed form; exact, never a substring.
        return _wait(tui, lambda s: unicodedata.normalize("NFC", _composer(s)) == expected, label)

    _send(tui, sent, "type", b"one two three")
    composer_is("one two three", "composer holds the typed words")
    _send(tui, sent, "Ctrl+W", CTRL_W)  # kills `three`, keeps the space (the row text is trimmed)
    composer_is("one two", "Ctrl+W removed the word before the cursor")
    _send(tui, sent, "Ctrl+A + X", CTRL_A)
    _send(tui, sent, "Ctrl+A + X", b"X")
    composer_is("Xone two", "Ctrl+A moved the cursor to the line start")
    _send(tui, sent, "Ctrl+E + Y", CTRL_E)
    _send(tui, sent, "Ctrl+E + Y", b"Y")
    composer_is("Xone two Y", "Ctrl+E moved the cursor to the line end")
    _send(tui, sent, "Ctrl+A, 3 x Right, Ctrl+K", CTRL_A + RIGHT.encode() * 3 + CTRL_K)
    composer_is("Xon", "Ctrl+K killed from the cursor to the line end")
    _send(tui, sent, "type", b"e rest")
    composer_is("Xone rest", "text typed at the end")
    _send(tui, sent, "5 x Left, Ctrl+U", LEFT.encode() * 5 + CTRL_U)
    composer_is("rest", "Ctrl+U killed from the line start to the cursor")
    _send(tui, sent, "Ctrl+E, a, e + combining acute", CTRL_E + "ae\u0301".encode())
    composer_is("resta\u00e9", "the accented letter typed")
    _send(tui, sent, "Ctrl+H", CTRL_H)
    screen = composer_is("resta", "Ctrl+H deleted one grapheme (the letter with its accent)")
    _capture(tui, evidence, "b-01-readline", screen)
    _send(tui, sent, "Ctrl+A, Ctrl+K", CTRL_A + CTRL_K)
    composer_is("", "composer cleared")

    # A turn held mid-stream, then Help over it, then a raw Ctrl+C.
    _send(tui, sent, "type held prompt", HOLD_PROMPT.encode())
    composer_is(HOLD_PROMPT, "held prompt typed")
    _send(tui, sent, "Enter", b"\r")
    screen = _wait(tui, lambda s: _composer(s) == "" and HELD_PARTIAL in s and "Ready" not in harness.status_row(s),
                   "the held stream is live: partial text painted, status not idle")
    with lock:
        assert [turn["held"] for turn in turns] == [True], turns
    _capture(tui, evidence, "b-02-stream-held", screen)

    witness, needle, footer, rail = OVERLAYS[HELP]
    _open_leader(tui, sent, "Ctrl+X ?")
    _send(tui, sent, "Ctrl+X ?", b"?")
    screen = _wait(tui, lambda s: _box(s, needle, footer, rail) is not None and not _menu(s),
                   f"{witness!r} overlay complete over the held stream")
    _capture(tui, evidence, "b-03-help-over-stream", screen)
    _send(tui, sent, "Ctrl+C in Help", b"\x03")
    screen = _wait(tui, lambda s: needle not in s and footer not in s and HELD_PARTIAL in s
                   and "Ready" not in harness.status_row(s),
                   "Ctrl+C closed Help; the held stream is still live")
    assert "[interrupted]" not in screen, screen
    _capture(tui, evidence, "b-04-help-closed-stream-live", screen)
    release.set()
    screen = _wait(tui, lambda s: HELD_TAIL in s and "Ready" in harness.status_row(s),
                   "the held stream finished after Ctrl+C in Help")
    assert "[interrupted]" not in screen, screen
    _capture(tui, evidence, "b-05-stream-finished", screen)
    assert tui.child.isalive()

    # `Ctrl+X Ctrl+Q` (3-B1 AC20a: the leader quits like a direct Ctrl+Q): the process exits 0, a
    # `System · Quit` decision (work or a draft) is answered `y` once.
    _open_leader(tui, sent, row["key"])
    _send(tui, sent, row["key"], bytes.fromhex(QUIT)[1:])
    deadline, answered = time.monotonic() + 30, False
    while True:
        try:
            data = tui.child.read_nonblocking(size=65536, timeout=1)
        except pexpect.TIMEOUT:
            assert time.monotonic() < deadline, f"{row['key']}: the process did not exit"
            continue
        except pexpect.EOF:
            break
        tui._stream.feed(data)
        if not answered and "System · Quit" in "\n".join(tui._screen.display):
            _send(tui, sent, "y", b"y")
            answered = True
    tui.child.close()
    assert (tui.child.exitstatus, tui.child.signalstatus) == (0, None)


def _exercise(binary, evidence, root, monkeypatch, rows):
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    errors = queue.Queue()
    posts = []
    turns = []  # leg B provider turns (held / auxiliary), never part of leg A's probe-only `posts`
    rejections = []  # leg C: provider requests that carried the plan rejection
    release = threading.Event()  # lets the held leg B reply finish
    turn_phase = threading.Event()  # set when leg B starts: only then is a streamed request legitimate
    lock = threading.Lock()
    ws_main, ws_panels, ws_edit, ws_plan = root / "wm", root / "wp", root / "we", root / "wc"
    for ws in (ws_main, ws_panels, ws_edit, ws_plan):
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
                if "stream" in body and turn_phase.is_set():
                    text = _user_text(body)
                    held = HOLD_PROMPT in text
                    plan = PLAN_PROMPT in text
                    with lock:
                        turns.append({"held": held, "text": text[:80]})
                        if "Plan rejected by user" in json.dumps(body.get("messages", [])[-1:]):
                            rejections.append(text[:80])
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.send_header("Transfer-Encoding", "chunked")
                    self.end_headers()
                    if plan:
                        self.wfile.write(_chunk(_plan_stream()) + b"0\r\n\r\n")
                        self.wfile.flush()
                        return
                    self.wfile.write(_chunk(_start_text(HELD_PARTIAL if held else "B session")))
                    self.wfile.flush()
                    if held:
                        assert release.wait(90), "the held reply was never released"
                        tail = {"type": "content_block_delta", "index": 0,
                                "delta": {"type": "text_delta", "text": f" {HELD_TAIL}"}}
                        self.wfile.write(_chunk(_event(tail) + _finish()))
                    else:
                        self.wfile.write(_chunk(_finish()))
                    self.wfile.write(b"0\r\n\r\n")
                    self.wfile.flush()
                    return
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
            release.set()
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

        envs = {"main": environment("main", ws_main), "panels": environment("panels", ws_panels),
                "edit": environment("edit", ws_edit), "plan": environment("plan", ws_plan)}
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

        def start(tag, ws, **kwargs):
            spawn_tag["next"] = tag
            tui = _NativeTUI(binary=binary, build=False, workspace=ws, env_overrides=envs[tag], fresh=True, **kwargs)
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

        # ------------------------------------------------------ leg B (3-B1): readline bytes, Help over a held stream
        turn_phase.set()
        edit = start("edit", ws_edit)
        try:
            _leg_b(edit, evidence, sent, release, turns, lock, rows[QUIT])
        finally:
            edit.stop()

        # ------------------------------------------------------ leg C (3-C): a pending plan card
        plan_tui = start("plan", ws_plan, allowed_tools=["propose_plan"])
        try:
            _leg_c(plan_tui, evidence, sent, rejections)
        finally:
            plan_tui.stop()

        # ---------------------------------------------------------- evidence checks
        with lock:
            observed = list(posts)
        (evidence / "requests.json").write_text(json.dumps(observed, indent=2) + "\n")
        (evidence / "sent.json").write_text(json.dumps(sent, indent=2) + "\n")
        with lock:
            (evidence / "turns.json").write_text(json.dumps(turns, indent=2) + "\n")
        assert len(rejections) == 1, f"the plan was rejected once, not {len(rejections)} times: {rejections}"
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
                     evidence / "requests.json", evidence / "sent.json", evidence / "turns.json", *logs,
                     *evidence.glob("*.screen.txt")]:
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


def _stripped(text):
    return "".join(text.split())


def _permission_wait(tui, predicate, label, timeout=60):
    """Leg D waits on PTY frames, never the approval helper's polling sleep."""
    deadline = time.monotonic() + timeout
    while True:
        screen = tui.get_screen_text()
        if predicate(screen):
            return screen
        assert time.monotonic() < deadline, f"Timed out waiting for {label}:\n{screen}"


class _PermissionTUI(_NativeTUI):
    """Attached-only spawn into `_world`'s prepared workspace; no startup fallback sleep."""

    def start(self):
        self._binary = harness._resolve_binary()[0].resolve()
        assert self._binary.is_file() and "debug" in self._binary.parts and "release" not in self._binary.parts
        self._workspace_path = self.workspace
        env = {**harness._load_env(), **self.env_overrides}
        env.setdefault("RUST_LOG", "debug")
        self._screen = harness.TerminalScreen(harness.TERM_COLS, harness.TERM_ROWS)
        self._stream = harness.pyte.Stream(self._screen)
        self._child = pexpect.spawn(str(self._binary), args=["daemon", "attach"], cwd=str(self.workspace),
                                    env=env, timeout=self.timeout, encoding="utf-8",
                                    dimensions=(harness.TERM_ROWS, harness.TERM_COLS))
        self.child.delaybeforesend = 0
        _permission_wait(self, lambda s: "Ready" in s, "attached terminal ready")
        return self

    def _sync_screen(self):
        try:
            data = self.child.read_nonblocking(size=65536, timeout=0.2)
        except pexpect.TIMEOUT:
            assert self.child.isalive(), "attached TUI exited unexpectedly"
            return
        except pexpect.EOF:
            pytest.fail("attached TUI exited before the expected screen")
        self._stream.feed(data)

    def send(self, data):
        os.write(self.child.child_fd, data.encode("utf-8"))


def _permission_daemon(stack, ws, env, tmp_path):
    """Bound readiness by a real daemon socket accepting and sending its challenge."""
    binary = harness._resolve_binary()[0].resolve()
    assert binary.is_file() and "debug" in binary.parts and "release" not in binary.parts
    log = stack.enter_context((tmp_path / "daemon.log").open("w"))
    proc = subprocess.Popen([str(binary), "daemon", "start", "--foreground"], cwd=ws, env=env,
                            stdout=log, stderr=subprocess.STDOUT, start_new_session=True)

    def stop():
        if proc.poll() is None:
            os.killpg(proc.pid, signal.SIGTERM)
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(proc.pid, signal.SIGKILL)
        proc.wait(timeout=5)

    stack.callback(stop)
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        assert proc.poll() is None, (tmp_path / "daemon.log").read_text()
        for path in (Path(env["RUSTAIN_DATA_DIR"]) / "daemons").glob("*.sock"):
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                break
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as probe:
                probe.settimeout(min(0.2, remaining))
                try:
                    probe.connect(str(path))
                    if probe.recv(1):
                        return
                except (FileNotFoundError, ConnectionRefusedError, socket.timeout):
                    pass
    pytest.fail("Daemon socket did not become ready:\n" + (tmp_path / "daemon.log").read_text())


@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(300)
def test_keymap_truth_attached_permission_card_takes_chat_focus_80x24(tmp_path, monkeypatch):
    """Leg D (3-C patch round, FR-21-3-c, AC19): the permission card natively, attached at 80x24."""
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    canary = tmp_path / "canary.txt"
    invocation = {"command": "printf leg-d >> " + shlex.quote(str(canary))}
    evidence = _Evidence("21-3-leg-d")
    sent = []
    with _world(tmp_path, "Bash", invocation, evidence) as (stack, ws, stub, env):
        assert env["ANTHROPIC_API_KEY"] == "operator" and env["ANTHROPIC_BASE_URL"].startswith("http://127.0.0.1:")
        # A daemon socket path must stay under SUN_LEN: the pytest tmp tree is too long on a deep TMPDIR.
        data = Path(tempfile.mkdtemp(prefix="r213d-"))
        stack.callback(shutil.rmtree, data, ignore_errors=True)
        env["RUSTAIN_DATA_DIR"] = str(data)
        _permission_daemon(stack, ws, env, tmp_path)
        tui = _PermissionTUI(build=False, fresh=False, workspace=ws, allowed_tools=[], env_overrides=env)
        stack.callback(tui.stop)
        tui.start()
        evidence.terminal(stack, tui, "writer.terminal.txt")

        def wait(predicate, what):
            return _permission_wait(tui, predicate, what)

        # `x` typed before any card is text in the composer (Input focus), then erased.
        sent.append("x")
        tui.send("x")
        wait(lambda s: _composer(s) == "x" and not tui._screen.cursor.hidden, "`x` is composer text in Input focus")
        sent.append("Backspace")
        tui.send("\x7f")
        wait(lambda s: _composer(s) == "", "composer empty again")
        # The approval arrives with an empty composer: the card takes Chat focus (cursor hidden)
        # and paints the permission rail.
        _command(tui)
        screen = wait(lambda s: "[r] Review" in s and "[n] Deny" in s, "the approval card is painted")
        evidence.text("d-01-card.screen.txt", screen)
        assert tui._screen.cursor.hidden, f"the arrival took Chat focus:\n{screen}"
        for rail in ("[y] Allow", "[s] Session", "[a] Always", "[n] Deny", "[f] Deny + feedback", "[r] Review"):
            assert _stripped(rail) in _stripped(screen), (rail, screen)
        assert not canary.exists()
        # Esc: one cue naming the explicit keys inside the clip; the card stays pending.
        sent.append("Esc")
        tui.send("\x1b")
        screen = wait(_cue, "Esc posts the cue")
        evidence.text("d-02-esc-cue.screen.txt", screen)
        cue_row = next(row for row in harness.toast_rows(screen, 3) if "never decide this card" in row)
        assert "Press [y] Allow  [n] Deny" in cue_row, cue_row
        assert "[y] Allow" in screen and not canary.exists(), screen
        # Ctrl+C neither interrupts nor quits and decides nothing.
        sent.append("Ctrl+C")
        tui.send("\x03")
        # The cue toast covers the card's second rail row at 80x24; the first row is the pending witness.
        screen = wait(lambda s: "[y] Allow" in s and _cue(s), "Ctrl+C leaves the card pending")
        assert tui.child.isalive() and "[interrupted]" not in screen and "System · Quit" not in screen, screen
        assert not _finished(stub), "no decision was sent"
        # The explicit key denies, once.
        sent.append("n")
        tui.send("n")
        wait(lambda s: _finished(stub), "the provider receives the denial")
        screen = wait(lambda s: "[y] Allow" not in s, "the card is gone")
        evidence.text("d-03-denied.screen.txt", screen)
        assert not canary.exists(), "denial must execute nothing"
        results = [row for row in stub.rows() if row.get("turn") == 2 and row.get("tool_results") == 1]
        assert len(results) == 1, f"the provider saw exactly one decision: {stub.rows()}"
        evidence.text("sent.json", json.dumps(sent, indent=2))
    for log in sorted(tmp_path.glob("runtime.log.*")) + [tmp_path / "daemon.log"]:
        if log.is_file():
            evidence.text(log.name, log.read_text(errors="replace"))
    if evidence.root:
        assert any(p.name.startswith("runtime.log.") for p in evidence.root.iterdir()), "dated runtime log missing"
        # The persona name is the loopback "key"; a leak means a real developer credential.
        secrets = [value for name, value in harness._load_env().items()
                   if len(value) >= 16 and re.search(r"KEY|TOKEN|SECRET", name, re.I)]
        for path in evidence.root.iterdir():
            text = path.read_text(errors="replace")
            assert not any(secret in text for secret in secrets), f"credential leaked to {path}"
