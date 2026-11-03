"""21.4 smoke legs A1/A2a/A2b: debug binary, both hosts, one 80x24 viewport.

No API/new marker, sleeps, imported driving helpers or canonical receipts. A
held loopback model turn crosses a tiny budget over scrolled Help. Unknown local
slash commands / attached-local refusals create notices; a draft guards quit.
Structural waits require the whole box, footer, status and cursor, not a title
substring. Raw sent/received bytes, settled screens, dated logs and binary SHA
stay in an env-scoped scratch run; publication is the parent's responsibility.
RUSTAIN_MODAL_STACK_DESKCHECK=1 exercises predicates OFFLINE (no binary/spawn).
"""
from contextlib import ExitStack
from datetime import datetime, timezone
import codecs
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import queue
import re
import runpy
import signal
import socket
import subprocess
import tempfile
import threading
import time
import traceback
from types import SimpleNamespace

import harness
import pexpect
import pytest

KEY = "sk-ant-21-4-loopback-key-canary"
NOTICES = ("a1-first-invalid", "a1-second-invalid", "a1-third-invalid")
DRAFT = "A1-kept-draft"
LOG = "System · Notifications"
QUIT = "System · Notifications › System · Quit"
HELP = "Help — Rustain Keybindings"
HELP_LOG = HELP+" › System · Notifications"
HELP_QUIT = HELP+" › System · Quit"
BUDGET = HELP+" › System · Daily budget"
BUDGET_RAIL = ("[y] Continue", "[s] Switch model", "[p] Pause until tomorrow", "[Esc] Later")
NO_DECISIONS = "No decisions waiting in this client"
TURN = "A2b-held-budget"
REPLY = "A2b budget completion"
DEFAULT_EVIDENCE = Path(tempfile.gettempdir()) / "rustain-21-4-smoke"
SCRATCH = Path(tempfile.gettempdir())
# A long TMPDIR is fine for evidence, but not for an AF_UNIX workspace socket.
if len(os.fsencode(SCRATCH / "r214-12345678/a/data/daemons" / ("0"*16+".sock"))) >= 108:
    SCRATCH = Path("/tmp")


def _evidence_parent():
    """Keep the explicit publication override independent of workspace sockets."""
    return Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", DEFAULT_EVIDENCE)).resolve()


def _box(screen, title, rail, closing=None):
    """Require the exact A8 sheet/centered rectangle, including every border."""
    rows = screen.split("\n")
    if len(rows) not in (16, 24, 30) or any(len(row) != len(rows[0]) for row in rows):
        return None
    width, region_height = len(rows[0]), len(rows)-4
    if width < 60:
        return None
    box_width, box_height = ((width, region_height) if width < 100 else
                            (width*9//10, min(region_height, max(5, region_height*85//100))))
    left, top = (width-box_width)//2, (region_height-box_height)//2
    box = [row[left:left+box_width] for row in rows[top:top+box_height]]
    if not (box[0].startswith("┌ "+title+" ") and box[0].endswith("┐")):
        return None
    if not (box[-1].startswith("└") and box[-1].endswith("┘")):
        return None
    if closing is not None and closing not in box[-1]:
        return None
    if closing is None and box[-1] != "└"+"─"*(box_width-2)+"┘":
        return None
    if any(not (row.startswith("│") and row.endswith("│")) for row in box[1:-1]):
        return None
    body = tuple(box[1:-1])
    content = [row[1:-1].strip() for row in body if row[1:-1].strip()]
    return body if not rail or (content and rail in content[-1]) else None


def _status(screen, state="Ready"):
    rows = screen.split("\n")
    return (len(rows) in (16, 24, 30) and harness.composer_top(rows) == len(rows)-3
            and state in rows[-4] and rows[-1].startswith("└") and rows[-1].endswith("┘"))


def _composer(screen, state="Ready"):
    rows = screen.split("\n")
    return (rows[-2][1:-1].strip() if _status(screen, state)
            and rows[-2].startswith("│") and rows[-2].endswith("│") else None)


def _help_box(screen):
    return _box(screen, HELP, "", closing="j/k scroll  ? close")


def _help_scrolled(screen):
    """The third scroll's final body row must arrive; the border is unchanged."""
    body = _help_box(screen)
    if body is None:
        return False
    first, last = body[0][1:-1].strip(), body[-1][1:-1].strip()
    width = len(screen.split("\n", 1)[0])
    if width == 60:
        return first.startswith("Ctrl+X chords.") and last == ""
    if width == 80:
        return first == "close." and last.startswith("Hold Shift    Select text for copy")
    if width == 130:
        return first == "" and last.startswith("G             Jump to newest message")
    return False


def _bottom_box(screen, title, width, bottom, closing=None):
    """Exact centered, bottom-anchored rectangle; never accept a partial border."""
    rows = screen.split("\n")
    if len(rows) not in (16, 24, 30) or any(len(row) != len(rows[0]) for row in rows):
        return None
    left = (len(rows[0])-width)//2
    tops = [y for y in range(bottom-2)
            if rows[y][left:left+width].startswith("┌ "+title+" ")]
    if len(tops) != 1:
        return None
    box = tuple(row[left:left+width] for row in rows[tops[0]:bottom])
    prefix = "┌ "+title+" "
    if box[0] != prefix+"─"*(width-len(prefix)-1)+"┐":
        return None
    if closing is None:
        if box[-1] != "└"+"─"*(width-2)+"┘":
            return None
    elif not (box[-1].startswith("└") and box[-1].endswith("┘") and closing in box[-1]):
        return None
    if any(not (row.startswith("│") and row.endswith("│")) for row in box[1:-1]):
        return None
    return box


def _cue(screen):
    match = re.search(r"Needs you \((\d+)\) \^X q", screen.split("\n")[-4])
    return int(match[1]) if match else None


def _budget_box(screen):
    rows = screen.split("\n")
    box = _bottom_box(screen, BUDGET, min(100, len(rows[0])), len(rows)-4)
    if not box:
        return None
    content = [row[1:-1].strip() for row in box[1:-1] if row[1:-1].strip()]
    text = " ".join(content)
    return box if (content and "Daily budget ($0.00) reached." in text
                   and all(pair in text for pair in BUDGET_RAIL)
                   and content[-1].endswith("[Esc] Later")) else None


def _popup_box(screen):
    rows = screen.split("\n")
    box = _bottom_box(screen, "Commands", len(rows[0]), len(rows)-4)
    return box if box and len(box) == 3 and box[-2][1:-1].strip().startswith("/detach  ") else None


def _leader_box(screen, feedback=None):
    rows = screen.split("\n")
    box = _bottom_box(screen, "Ctrl+X", len(rows[0]), len(rows)-4, feedback)
    return box if box and "[Esc] Cancel" in box[-2] else None


def _no_decisions_menu(screen):
    return _leader_box(screen, NO_DECISIONS)


def _notice_ready(screen, command, attached=False):
    """Unique surviving toast prefix plus an empty, fully settled composer."""
    rows = screen.split("\n")
    top = harness.composer_top(rows)
    if (top != len(rows)-3 or not rows[top].endswith("┐")
            or not rows[-1].startswith("└") or not rows[-1].endswith("┘")
            or rows[top+1] != "│"+" "*(len(rows[top+1])-2)+"│"
            or not re.search(r"│ Ready(?:[ │]|$)", rows[top-1])):
        return False
    text = (f"Info {command} is not available" if attached
            else f"Warning Unknown command: {command}")
    prefix = re.compile(rf"^\s*│?\s*{re.escape(text)}(?:\s|…)")
    return any(prefix.match(row) for row in harness.toast_rows(screen, rows=3))


def _selected(tui, notices=NOTICES):
    """Selection is the actual reverse-video record, never a matching toast."""
    for y, row in enumerate(tui.screen.display[:20]):
        for marker in notices:
            x = row.find(marker)
            if x >= 0 and any(tui.screen.buffer[y][c].reverse for c in range(x, x+len(marker))):
                return marker
    return None


def _deskcheck():
    # Positive and negative controls for torn frames, hidden footer and status occlusion.
    def fixture(title=LOG, rail="[Enter] List  [Esc] Close"):
        rows = ["┌ "+title+" "+"─"*(76-len(title))+"┐"]
        rows += ["│"+" "*78+"│" for _ in range(18)]
        rows[2] = "│"+"Notifications (3) · detail".ljust(78)+"│"
        rows[17] = "│"+rail.ljust(78)+"│"
        rows += ["└"+"─"*78+"┘", "Ready".ljust(80), "┌"+"─"*78+"┐",
                 "│"+DRAFT.ljust(78)+"│", "└"+"─"*78+"┘"]
        return rows
    rows = fixture()
    screen = "\n".join(rows)
    assert _box(screen, LOG, "[Esc] Close") and _status(screen) and _composer(screen) == DRAFT
    assert _box("\n".join(fixture(QUIT)), QUIT, "[Esc] Close")
    for index, replacement in [(0, " "*80), (9, "│"+" "*79), (17, "│"+" "*78+"│"), (19, " "*80)]:
        torn = rows.copy(); torn[index] = replacement
        assert _box("\n".join(torn), LOG, "[Esc] Close") is None
    rows[20] = "System · Quit".ljust(80)
    assert not _status("\n".join(rows))
    final_quit = "\n".join(fixture("System · Quit", "[y] Quit  [n/Esc/Enter] Stay"))
    assert _box(final_quit, "System · Quit", "Stay") and _status(final_quit)
    rows = fixture()
    rows[23] = " "*80
    assert _composer("\n".join(rows)) is None
    selected = harness.TerminalScreen(80, 24)
    stream = harness.pyte.Stream(selected)
    stream.feed("\x1b[2;1H"+NOTICES[1])
    assert _selected(SimpleNamespace(screen=selected)) is None
    stream.feed("\x1b[2;1H\x1b[7m"+NOTICES[1]+"\x1b[0m")
    assert _selected(SimpleNamespace(screen=selected)) == NOTICES[1]
    socket_path = SCRATCH / "r214-12345678/a/data/daemons" / ("0"*16+".sock")
    assert len(os.fsencode(socket_path)) < 108, "scratch socket path exceeds AF_UNIX budget"
    _deskcheck_help_frames()
    _deskcheck_a2b_frames()
    _deskcheck_saved_frames()


def _deskcheck_help_frames():
    """Use saved real TestHarness frames at all three named viewports."""
    path = Path(os.environ["RUSTAIN_MODAL_STACK_HELP_FRAMES"])
    frames = {}
    for line in path.read_text().splitlines():
        if line.startswith("HELP_DESKCHECK_FRAME="):
            width, height, stage, screen = json.loads(line.split("=", 1)[1])
            assert len(screen.split("\n")) == height
            assert all(len(row) == width for row in screen.split("\n"))
            frames[width, height, stage] = screen
    assert len(frames) == 21, frames.keys()
    for width, height in [(60, 16), (80, 24), (130, 30)]:
        def frame(stage):
            screen = frames[width, height, stage]
            assert _status(screen) and _composer(screen) == DRAFT, (width, height, stage, screen)
            return screen
        initial = frame("help-initial")
        scrolled = frame("help-scrolled")
        assert _help_box(initial) and _help_box(scrolled), (width, height)
        assert _help_box(initial) != _help_box(scrolled)
        assert not _help_scrolled(initial)
        assert _help_scrolled(scrolled), (width, height)
        assert _box(frame("help-log"), HELP_LOG, "[Esc] Close")
        assert _box(frame("help-quit"), HELP_QUIT, "[n/Esc/Enter] Stay")
        assert _help_box(frame("help-restored-log")) == _help_box(scrolled)
        assert _help_box(frame("help-restored-quit")) == _help_box(scrolled)
        assert HELP not in frame("composer")
        # Independent named-viewport geometry oracle; reject a shifted title
        # and a complete border whose Help footer has not settled.
        left, top, box_width, box_height = ((0, 0, width, height-4) if width < 100 else (6, 2, 117, 22))
        rows = initial.split("\n")
        assert rows[top].index("┌") == left
        assert rows[top].index("┐") == left+box_width-1
        assert rows[top+box_height-1][left] == "└"
        broken = rows.copy(); broken[top] = " "+broken[top][:-1]
        assert _help_box("\n".join(broken)) is None
        broken = rows.copy()
        footer = top+box_height-1
        broken[footer] = broken[footer].replace("j/k scroll  ? close", " "*len("j/k scroll  ? close"))
        assert _help_box("\n".join(broken)) is None


def _deskcheck_a2b_frames():
    """Rust renders are positive controls; damage each required structural edge."""
    frames = {}
    for line in Path(os.environ["RUSTAIN_MODAL_STACK_A2B_FRAMES"]).read_text().splitlines():
        if line.startswith("A2B_DESKCHECK_FRAME="):
            width, height, stage, screen = json.loads(line.split("=", 1)[1])
            assert len(screen.split("\n")) == height
            assert all(len(row) == width for row in screen.split("\n"))
            assert (width, height, stage) not in frames
            frames[width, height, stage] = screen
    assert len(frames) == 24, frames.keys()
    for width, height in [(60, 16), (80, 24), (130, 30)]:
        def frame(stage, draft=""):
            screen = frames[width, height, stage]
            assert _status(screen) and _composer(screen) == draft, (width, height, stage, screen)
            return screen
        before = frame("help-before-arrival")
        assert _help_scrolled(before) and _cue(before) is None
        for stage in ("help-budget", "help-reopened-budget"):
            screen = frame(stage)
            box = _budget_box(screen)
            assert box and _cue(screen) == 1, (width, height, stage, screen)
            top = height-4-len(box)
            left = (width-min(100, width))//2
            rail_y = next(y for y in range(top+1, height-5) if "[Esc] Later" in screen.split("\n")[y])
            for y, x in [(top, left), (top+1, left), (height-5, left), (rail_y, left+1)]:
                rows = screen.split("\n")
                rows[y] = rows[y][:x]+" "+rows[y][x+1:]
                if y == rail_y:
                    rows[y] = rows[y].replace("[Esc] Later", " "*len("[Esc] Later"))
                assert _budget_box("\n".join(rows)) is None, (width, height, y, x)
            rows = screen.split("\n"); rows[-4] = " "*width
            assert not _status("\n".join(rows))
        for stage, cue in [("help-deferred", 1), ("help-resolved", None)]:
            screen = frame(stage)
            assert _help_box(screen) == _help_box(before) and _cue(screen) == cue
            assert BUDGET not in screen
        popup = frame("attached-popup", "/det")
        assert _popup_box(popup) and _composer(popup) == "/det"
        for y in (height-6, height-5):
            rows = popup.split("\n"); rows[y] = " "*width
            assert _popup_box("\n".join(rows)) is None
        rollback = frame("attached-popup-rollback")
        assert "┌ Commands " not in rollback and _composer(rollback) == ""
        menu = frame("attached-help-no-decisions")
        assert _no_decisions_menu(menu) and _cue(menu) is None and BUDGET not in menu
        rows = menu.split("\n"); rows[-5] = " "*width
        assert _no_decisions_menu("\n".join(rows)) is None


def _deskcheck_saved_frames():
    """Reuse published A2a native screens, independently of Rust fixture output."""
    root = Path(os.environ["RUSTAIN_MODAL_STACK_SAVED_FRAMES"])
    for tag in ("local", "attached"):
        screens = [(root / f"{tag}-{label}.screen.txt").read_text().removesuffix("\n")
                   for label in ("06-help-scrolled", "08-exact-help-from-log", "10-exact-help-from-quit")]
        assert all(_help_scrolled(screen) and _status(screen) and _composer(screen) == DRAFT
                   for screen in screens), tag
        assert all(_help_box(screen) == _help_box(screens[0]) for screen in screens), tag
        rows = screens[0].split("\n"); rows[19] = " "*80
        assert _help_box("\n".join(rows)) is None


class _Terminal:
    def __init__(self, stack, binary, args, ws, env, evidence, tag, errors):
        self.tag, self.evidence, self.errors = tag, evidence, errors
        self.screen = harness.TerminalScreen(80, 24)
        self.stream = harness.pyte.Stream(self.screen)
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self.sent = []
        self.frames = stack.enter_context((evidence / f"{tag}.screens.jsonl").open("w"))
        raw = stack.enter_context((evidence / f"{tag}.terminal.bin").open("wb"))
        self.child = pexpect.spawn(str(binary), args=args, cwd=str(ws), env=env,
                                   encoding=None, dimensions=(24, 80), timeout=60)
        self.child.logfile_read = raw
        self.child.delayafterclose = self.child.delayafterterminate = 0
        stack.callback(self.stop)

    def read(self, timeout=0.2):
        try:
            data = self.child.read_nonblocking(65536, timeout=timeout)
        except pexpect.TIMEOUT:
            assert self.child.isalive(), f"{self.tag} exited unexpectedly"
            return
        self.stream.feed(self.decoder.decode(data))
        self.frames.write(json.dumps({"at": time.monotonic(), "screen": "\n".join(self.screen.display),
                                     "cursor_hidden": self.screen.cursor.hidden})+"\n")
        self.frames.flush()

    def wait(self, predicate, label, timeout=60):
        deadline = time.monotonic()+timeout
        # At least one PTY read after every input prevents accepting a stale screen.
        while time.monotonic() < deadline:
            if not self.errors.empty():
                raise AssertionError(self.errors.get_nowait())
            self.read(min(0.2, max(0, deadline-time.monotonic())))
            screen = "\n".join(self.screen.display)
            if predicate(screen):
                return screen
        raise AssertionError(f"{self.tag}: timed out: {label}\n"+"\n".join(self.screen.display))

    def settle(self, predicate, seconds, label):
        """Observe bounded structural snapshots beyond a guard, never sleep/replay."""
        self.wait(predicate, label)
        after = time.monotonic()+seconds
        return self.wait(lambda s: predicate(s) and time.monotonic() >= after, label, timeout=seconds+5)

    def send(self, label, data):
        self.sent.append({"label": label, "hex": data.hex()})
        (self.evidence / f"{self.tag}.sent.json").write_text(json.dumps(self.sent, indent=2)+"\n")
        assert os.write(self.child.child_fd, data) == len(data)

    def capture(self, label, screen):
        (self.evidence / f"{self.tag}-{label}.screen.txt").write_text(screen+"\n")

    def exit(self):
        self.send("guarded final quit", b"\x11")
        self.wait(lambda s: _box(s, "System · Quit", "Stay") and _status(s), "settled final quit frame")
        self.send("confirm final quit", b"y")
        deadline = time.monotonic()+30
        while time.monotonic() < deadline:
            try:
                self.read()
            except pexpect.EOF:
                self.child.close()
                assert (self.child.exitstatus, self.child.signalstatus) == (0, None)
                return
        raise AssertionError(f"{self.tag}: explicit quit did not exit")

    def stop(self):
        # Only the child/group we own; no shell pkill and no key-based teardown.
        if self.child.isalive():
            group = os.getpgid(self.child.pid)
            kill = lambda sig: os.killpg(group, sig) if group == self.child.pid else os.kill(self.child.pid, sig)
            kill(signal.SIGTERM)
            try:
                self.child.expect(pexpect.EOF, timeout=5)
            except pexpect.TIMEOUT:
                kill(signal.SIGKILL)
                self.child.expect(pexpect.EOF, timeout=5)
        self.child.close()


def _daemon(stack, binary, ws, env, evidence):
    log = stack.enter_context((evidence / "daemon.stdout.txt").open("w"))
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
    deadline = time.monotonic()+30
    while time.monotonic() < deadline:
        assert proc.poll() is None, (evidence / "daemon.stdout.txt").read_text()
        for path in (Path(env["RUSTAIN_DATA_DIR"]) / "daemons").glob("*.sock"):
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as probe:
                probe.settimeout(min(0.2, max(0.001, deadline-time.monotonic())))
                try:
                    probe.connect(str(path))
                    if probe.recv(1):
                        return
                except (FileNotFoundError, ConnectionRefusedError, socket.timeout):
                    pass
    raise AssertionError("daemon did not send its socket challenge")


def _leg(tui, held, release):
    attached = tui.tag == "attached"
    notices = ("mode", "plan", "config") if attached else NOTICES
    tui.wait(lambda s: _status(s) and not tui.screen.cursor.hidden, "Input ready")
    for marker in notices:
        command = "/"+marker
        tui.send("type notice-producing slash plus delimiter", (command+" ").encode())
        tui.wait(lambda s: _composer(s) == command and tui.screen.cursor.x == len(command)+2,
                 "slash delimiter consumed; autocomplete closed")
        tui.send("submit notice-producing slash", b"\r")
        tui.wait(lambda s: _notice_ready(s, command, attached), "settled real notice prefix")
    tui.send("retained draft", DRAFT.encode())
    tui.wait(lambda s: _composer(s) == DRAFT, "draft retained")
    tui.send("Ctrl+X n", b"\x18n")
    screen = tui.wait(lambda s: _box(s, LOG, "[Esc] Close") and _status(s)
                      and all(marker in s for marker in notices) and _selected(tui, notices) == notices[2]
                      and tui.screen.cursor.hidden, "three real notices; newest selected")
    tui.capture("01-log-list", screen)
    tui.send("select non-newest", b"j")
    tui.wait(lambda s: _box(s, LOG, "[Esc] Close") and _selected(tui, notices) == notices[1], "second-newest selected")
    tui.send("open selected detail", b"\r")
    screen = tui.wait(lambda s: _box(s, LOG, "[Esc] Close") and re.search(r"Notifications \((?:[3-9]|\d{2,})\) · detail", s)
                      and notices[1] in s and notices[2] not in s and "[Enter] List" in s
                      and _status(s) and tui.screen.cursor.hidden, "non-newest detail complete")
    before = _box(screen, LOG, "[Esc] Close")
    status_prefix = screen.split("\n")[20].split("Ready", 1)[0]+"Ready"
    tui.capture("02-selected-detail", screen)
    tui.send("Ctrl+Q over log", b"\x11")
    screen = tui.wait(lambda s: _box(s, QUIT, "[n/Esc/Enter] Stay") and _status(s)
                      and s.split("\n")[20].startswith(status_prefix) and tui.screen.cursor.hidden,
                      "Log › Quit breadcrumb; intact status")
    tui.capture("03-log-quit", screen)
    tui.send("cancel quit only", b"n")
    screen = tui.wait(lambda s: _box(s, LOG, "[Esc] Close") == before and QUIT not in s
                      and _status(s) and s.split("\n")[20].startswith(status_prefix)
                      and tui.screen.cursor.hidden, "exact same non-newest detail restored")
    tui.capture("04-exact-detail-restored", screen)
    tui.send("Esc closes log", b"\x1b")
    screen = tui.wait(lambda s: LOG not in s and _status(s) and _composer(s) == DRAFT
                      and not tui.screen.cursor.hidden, "composer cursor visible; draft intact")
    tui.capture("05-composer-restored", screen)
    _help_leg(tui)
    _a2b_leg(tui, held, release)
    tui.exit()


def _help_leg(tui):
    tui.send("Ctrl+X ? opens Help", b"\x18?")
    initial = tui.wait(lambda s: _help_box(s) and _status(s) and tui.screen.cursor.hidden,
                       "complete Help with footer and intact status")
    tui.send("scroll Help three rows", b"jjj")
    screen = tui.wait(lambda s: _help_scrolled(s) and _status(s) and tui.screen.cursor.hidden,
                      "third Help scroll's final row settled")
    before = _help_box(screen)
    tui.capture("06-help-scrolled", screen)
    tui.send("Ctrl+X n from Help", b"\x18n")
    screen = tui.wait(lambda s: _box(s, HELP_LOG, "[Esc] Close") and _status(s)
                      and tui.screen.cursor.hidden, "Help › Log breadcrumb and rail")
    tui.capture("07-help-log", screen)
    tui.send("Esc restores Help", b"\x1b")
    screen = tui.wait(lambda s: _help_box(s) == before and _status(s)
                      and tui.screen.cursor.hidden, "exact scrolled Help restored from Log")
    tui.capture("08-exact-help-from-log", screen)
    tui.send("Ctrl+Q over Help", b"\x11")
    screen = tui.wait(lambda s: _box(s, HELP_QUIT, "[n/Esc/Enter] Stay") and _status(s)
                      and tui.screen.cursor.hidden, "Help › Quit breadcrumb and intact status")
    tui.capture("09-help-quit", screen)
    tui.send("cancel Quit over Help", b"n")
    screen = tui.wait(lambda s: _help_box(s) == before and _status(s)
                      and tui.screen.cursor.hidden, "exact scrolled Help restored from Quit")
    tui.capture("10-exact-help-from-quit", screen)
    tui.send("Esc closes Help", b"\x1b")
    screen = tui.wait(lambda s: HELP not in s and _status(s) and _composer(s) == DRAFT
                      and not tui.screen.cursor.hidden, "Help returns to draft and composer cursor")
    tui.capture("11-help-composer-restored", screen)


def _a2b_leg(tui, held, release):
    # A1/A2a retained the draft. Empty it only for the REAL model submission /
    # slash trigger; final restoration checks the same draft again before quit.
    tui.send("clear retained draft for A2b", b"\x7f"*len(DRAFT))
    tui.wait(lambda s: _composer(s) == "" and not tui.screen.cursor.hidden, "empty A2b composer")
    if tui.tag == "local":
        tui.send("real budget model turn", TURN.encode()+b"\r")
        tui.wait(lambda s: held.is_set() and _composer(s, "Streaming...") == "",
                 "real model request reached loopback and remains held")
    else:
        tui.send("attached routed slash filter", b"/det")
        screen = tui.wait(lambda s: _popup_box(s) and _composer(s) == "/det"
                          and not tui.screen.cursor.hidden, "complete attached slash popup")
        tui.capture("12-attached-popup", screen)
        tui.send("rollback provisional slash edit", b"\x1b")
        screen = tui.wait(lambda s: "┌ Commands " not in s and _composer(s) == ""
                          and not tui.screen.cursor.hidden, "exact slash rollback without leaving Input")
        tui.capture("13-attached-popup-rollback", screen)
    busy = "Streaming..." if tui.tag == "local" else "Ready"
    tui.send("A2b Help over held turn or attached composer", b"\x18?")
    tui.wait(lambda s: _help_box(s) and _status(s, busy) and tui.screen.cursor.hidden,
             "complete A2b Help")
    tui.send("A2b third Help scroll", b"jjj")
    screen = tui.wait(lambda s: _help_scrolled(s) and _status(s, busy) and tui.screen.cursor.hidden,
                      "A2b third scroll's exact final row")
    before = _help_box(screen)
    tui.capture("14-a2b-help-before", screen)
    if tui.tag == "local":
        # Last printable was j. Structural PTY reads pass the >1s arrival grace
        # with the provider blocked: arrival must PREEMPT without an explicit q.
        tui.settle(lambda s: held.is_set() and not release.is_set()
                   and _help_box(s) == before and _status(s, busy)
                   and tui.screen.cursor.hidden, 1.1, "held Help beyond printable grace")
        release.set()
        screen = tui.wait(lambda s: _budget_box(s) and _status(s) and _cue(s) == 1
                          and _composer(s) == "" and tui.screen.cursor.hidden,
                          "automatic budget preempts Help with breadcrumb, full rail and Ready cue")
        tui.capture("15-budget-preempts-help", screen)
        tui.settle(lambda s: _budget_box(s) and _status(s) and _cue(s) == 1
                   and tui.screen.cursor.hidden, 0.5, "painted budget beyond 500ms inert guard")
        tui.send("Esc defers only budget", b"\x1b")
        screen = tui.wait(lambda s: _help_box(s) == before and _status(s) and _cue(s) == 1
                          and BUDGET not in s and tui.screen.cursor.hidden,
                          "defer restores exact Help frame body scroll and keeps cue")
        tui.capture("16-help-budget-deferred", screen)
        tui.send("frame-origin Ctrl+X q reopens decision", b"\x18q")
        screen = tui.wait(lambda s: _budget_box(s) and _status(s) and _cue(s) == 1
                          and tui.screen.cursor.hidden, "frame-origin q reopens deferred budget")
        tui.capture("17-budget-reopened", screen)
        tui.settle(lambda s: _budget_box(s) and _status(s) and _cue(s) == 1
                   and tui.screen.cursor.hidden, 0.5, "reopened budget beyond its own 500ms guard")
        tui.send("resolve budget Continue once", b"y")
        screen = tui.wait(lambda s: _help_box(s) == before and _status(s) and _cue(s) is None
                          and BUDGET not in s and tui.screen.cursor.hidden,
                          "resolve restores exact Help and removes decision cue")
        tui.capture("18-help-budget-resolved", screen)
    else:
        tui.send("attached frame-origin leader", b"\x18")
        tui.wait(lambda s: _leader_box(s) and _status(s)
                 and tui.screen.cursor.hidden, "settled frame-origin leader over Help")
        tui.send("attached q is truthful local refusal", b"q")
        screen = tui.wait(lambda s: _no_decisions_menu(s) and _status(s) and _cue(s) is None
                          and BUDGET not in s and tui.screen.cursor.hidden,
                          "truthful No decisions waiting in this client in frame-origin menu; no pane")
        tui.capture("15-attached-no-decisions", screen)
        tui.send("cancel attached leader only", b"\x1b")
        screen = tui.wait(lambda s: _help_box(s) == before and _status(s)
                          and "┌ Ctrl+X " not in s and _cue(s) is None
                          and tui.screen.cursor.hidden, "attached leader returns exact Help")
        tui.capture("16-attached-exact-help", screen)
    tui.send("close A2b Help", b"\x1b")
    tui.wait(lambda s: HELP not in s and _composer(s) == ""
             and not tui.screen.cursor.hidden and (tui.tag != "local" or REPLY in s),
             "A2b Help closed to empty Input and completed real reply")
    tui.send("restore final retained draft", DRAFT.encode())
    screen = tui.wait(lambda s: _composer(s) == DRAFT and _cue(s) is None
                      and not tui.screen.cursor.hidden, "final retained draft and no decisions")
    tui.capture("19-a2b-final-draft", screen)


def test_scratch_paths_are_portable_and_socket_bounded(monkeypatch):
    """Filesystem-only controls: no binary resolution, server, socket or TUI."""
    with tempfile.TemporaryDirectory(prefix="r214-paths-", dir="/tmp") as directory:
        root = Path(directory)
        home = root / "nondeveloper-home"
        home.mkdir()
        monkeypatch.setenv("HOME", str(home))
        monkeypatch.delenv("RUSTAIN_CAPTURE_EVIDENCE_DIR", raising=False)
        for tmp in (root / "tmp", root / ("long-temporary-root-"*6)):
            tmp.mkdir()
            monkeypatch.setenv("TMPDIR", str(tmp))
            monkeypatch.setattr(tempfile, "tempdir", None)
            module = runpy.run_path(__file__, run_name="modal_stack_path_control")
            scratch, evidence = module["SCRATCH"], module["DEFAULT_EVIDENCE"]
            assert evidence.is_relative_to(tmp)
            assert module["_evidence_parent"]() == evidence.resolve()
            socket_path = tmp / "r214-12345678/a/data/daemons" / ("0"*16+".sock")
            assert scratch == (tmp if len(os.fsencode(socket_path)) < 108 else Path("/tmp"))
            assert not scratch.is_relative_to(home)
            evidence.mkdir(parents=True)
            with tempfile.TemporaryDirectory(prefix="r214-", dir=scratch) as workspace:
                for tag in ("l", "a"):
                    ws = Path(workspace) / tag
                    (ws / "data/daemons").mkdir(parents=True)
                    assert len(os.fsencode(ws / "data/daemons" / ("0"*16+".sock"))) < 108
            override = root / "explicit-evidence"
            monkeypatch.setenv("RUSTAIN_CAPTURE_EVIDENCE_DIR", str(override))
            assert module["_evidence_parent"]() == override.resolve()
            assert module["SCRATCH"] == scratch, "evidence override must not relocate sockets"
            monkeypatch.delenv("RUSTAIN_CAPTURE_EVIDENCE_DIR")


@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(240)
def test_modal_stack_a1_a2a_local_and_attached_80x24():
    if os.environ.get("RUSTAIN_MODAL_STACK_DESKCHECK") == "1":
        _deskcheck()
        return
    binary = harness._resolve_binary()[0].resolve()
    assert binary.is_file() and "debug" in binary.parts and "release" not in binary.parts, binary
    parent = _evidence_parent()
    parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="21-4-a2b-", dir=parent))
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    manifest = {"binary": str(binary), "sha256": digest, "bytes": binary.stat().st_size, "viewport": [80, 24],
                "started_at": datetime.now(timezone.utc).isoformat(), "result": "RUNNING", "legs": ["local", "attached"]}
    errors, posts = queue.Queue(), []
    held, release = threading.Event(), threading.Event()
    try:
        with ExitStack() as stack:
            class Handler(BaseHTTPRequestHandler):
                protocol_version = "HTTP/1.1"
                def log_message(self, *_args):
                    pass
                def reply(self, data):
                    self.send_response(200); self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(data))); self.end_headers(); self.wfile.write(data)
                def do_GET(self):
                    self.reply(b"{}" if self.path.split("?", 1)[0] == "/api.json" else b'{"data":[],"has_more":false}')
                def do_POST(self):
                    try:
                        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                        probe = body.get("max_tokens") == 1 and "stream" not in body
                        posts.append({"kind": "probe" if probe else "turn",
                                      "max_tokens": body.get("max_tokens"), "stream": body.get("stream", False)})
                        if probe:
                            self.reply(b'{"type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}')
                            return
                        assert body.get("stream") is True and TURN in json.dumps(body.get("messages", []))
                        assert sum(post["kind"] == "turn" for post in posts) == 1, "unexpected extra real turn"
                        held.set()
                        assert release.wait(90), "held A2b model turn was never released"
                        events = [
                            {"type": "message_start", "message": {"usage": {"input_tokens": 5}}},
                            {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}},
                            {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": REPLY}},
                            {"type": "content_block_stop", "index": 0},
                            {"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}},
                            {"type": "message_stop"},
                        ]
                        data = "".join(f"event: {event['type']}\ndata: {json.dumps(event)}\n\n" for event in events).encode()
                        self.send_response(200)
                        self.send_header("Content-Type", "text/event-stream")
                        self.send_header("Transfer-Encoding", "chunked")
                        self.end_headers()
                        self.wfile.write(f"{len(data):x}\r\n".encode()+data+b"\r\n0\r\n\r\n")
                        self.wfile.flush()
                    except BaseException:
                        errors.put(traceback.format_exc()); self.send_error(500)
            server = ThreadingHTTPServer(("127.0.0.1", 0), Handler); server.daemon_threads = True
            thread = threading.Thread(target=server.serve_forever, daemon=True); thread.start()
            def close_server():
                release.set()
                server.shutdown(); server.server_close(); thread.join(timeout=5)
                assert not thread.is_alive()
            stack.callback(close_server)
            SCRATCH.mkdir(parents=True, exist_ok=True)
            root = Path(stack.enter_context(tempfile.TemporaryDirectory(prefix="r214-", dir=SCRATCH)))
            (root / "home").mkdir()
            origin = f"http://127.0.0.1:{server.server_port}"
            def environment(tag, ws):
                config = ws / ".rustain"; config.mkdir(parents=True)
                settings = "[notifications]\ninfo_ttl_secs = 30\n"
                if tag == "local":
                    settings += ('\n[budget]\ndaily_limit_usd = 0.000001\n'
                                 '\n[pricing."claude-sonnet-4-6"]\ninput_per_million = 3.0\noutput_per_million = 15.0\n')
                (ws / ".env").write_text(""); (config / "config.toml").write_text(settings)
                return {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LANG": "C.UTF-8", "TERM": "xterm-256color",
                        "HOME": str(root / "home"), "XDG_CONFIG_HOME": str(root / "config"), "XDG_DATA_HOME": str(root / "xdg-data"),
                        "XDG_CACHE_HOME": str(root / "cache"), "RUSTAIN_CONFIG_DIR": str(config), "RUSTAIN_DATA_DIR": str(ws / "data"),
                        "RUSTAIN_LOG_PATH": str(evidence / f"{tag}.runtime.log"), "RUST_LOG": "debug",
                        "ANTHROPIC_API_KEY": KEY, "ANTHROPIC_BASE_URL": origin, "RUSTAIN_MODELS_DEV_URL": origin, "NO_PROXY": "*"}
            for tag in ("local", "attached"):
                ws = root / ("l" if tag == "local" else "a"); env = environment(tag, ws)
                assert len(os.fsencode(ws / "data/daemons" / ("0"*16+".sock"))) < 108
                if tag == "attached":
                    daemon_env = {**env, "RUSTAIN_LOG_PATH": str(evidence / "daemon.runtime.log")}
                    _daemon(stack, binary, ws, daemon_env, evidence)
                tui = _Terminal(stack, binary, ["--new"] if tag == "local" else ["daemon", "attach"], ws, env, evidence, tag, errors)
                _leg(tui, held, release)
        assert errors.empty(), list(errors.queue)
        assert held.is_set() and release.is_set()
        assert sum(post["kind"] == "turn" for post in posts) == 1, posts
        logs = sorted(evidence.glob("*.runtime.log.*"))
        for tag in ("local", "attached", "daemon"):
            assert any(p.name.startswith(tag+".runtime.log.") and re.fullmatch(r"\d{4}-\d{2}-\d{2}", p.suffix[1:]) for p in logs), f"missing dated {tag} debug log"
        (evidence / "requests.json").write_text(json.dumps(posts, indent=2)+"\n")
        for path in evidence.iterdir():
            assert KEY.encode() not in path.read_bytes(), f"credential leaked to {path.name}"
        assert hashlib.sha256(binary.read_bytes()).hexdigest() == digest, "binary changed during capture"
        manifest["result"] = "PASS"
    except BaseException:
        manifest["result"] = "FAIL"; (evidence / "failure.txt").write_text(traceback.format_exc())
        raise
    finally:
        manifest["completed_at"] = datetime.now(timezone.utc).isoformat()
        (evidence / "run.json").write_text(json.dumps(manifest, indent=2)+"\n")
        print(f"NATIVE_21_4_A2B_EVIDENCE={evidence}", flush=True)
