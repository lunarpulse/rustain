"""21.4 smoke legs A1/A2a: real debug binary, both hosts, one 80x24 viewport.

No API/new marker, sleeps, imported driving helpers or canonical receipts. A
loopback probe-only provider makes any model turn a failure. Unknown local slash
commands / attached-local refusals create notices; a retained draft guards quit.
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
DEFAULT_EVIDENCE = Path(tempfile.gettempdir()) / "rustain-21-4-smoke"


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


def _status(screen):
    rows = screen.split("\n")
    return (len(rows) in (16, 24, 30) and harness.composer_top(rows) == len(rows)-3
            and "Ready" in rows[-4] and rows[-1].startswith("└") and rows[-1].endswith("┘"))


def _composer(screen):
    rows = screen.split("\n")
    return (rows[-2][1:-1].strip() if _status(screen)
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
    socket_path = Path(tempfile.gettempdir()) / "r214-12345678/attached/data/daemons" / ("0"*16+".sock")
    assert len(os.fsencode(socket_path)) < 108, "scratch socket path exceeds AF_UNIX budget"
    _deskcheck_help_frames()


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


def _leg(tui):
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


@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(240)
def test_modal_stack_a1_a2a_local_and_attached_80x24():
    if os.environ.get("RUSTAIN_MODAL_STACK_DESKCHECK") == "1":
        _deskcheck()
        return
    binary = harness._resolve_binary()[0].resolve()
    assert binary.is_file() and "debug" in binary.parts and "release" not in binary.parts, binary
    parent = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", DEFAULT_EVIDENCE)).resolve()
    parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="21-4-a2a-", dir=parent))
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    manifest = {"binary": str(binary), "sha256": digest, "bytes": binary.stat().st_size, "viewport": [80, 24],
                "started_at": datetime.now(timezone.utc).isoformat(), "result": "RUNNING", "legs": ["local", "attached"]}
    errors, posts = queue.Queue(), []
    try:
        with ExitStack() as stack:
            class Handler(BaseHTTPRequestHandler):
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
                        posts.append({"max_tokens": body.get("max_tokens"), "stream": body.get("stream", False)})
                        assert body.get("max_tokens") == 1 and not body.get("stream"), "unexpected real model turn"
                        self.reply(b'{"type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}')
                    except BaseException:
                        errors.put(traceback.format_exc()); self.send_error(500)
            server = ThreadingHTTPServer(("127.0.0.1", 0), Handler); server.daemon_threads = True
            thread = threading.Thread(target=server.serve_forever, daemon=True); thread.start()
            def close_server():
                server.shutdown(); server.server_close(); thread.join(timeout=5)
                assert not thread.is_alive()
            stack.callback(close_server)
            root = Path(stack.enter_context(tempfile.TemporaryDirectory(prefix="r214-", dir=tempfile.gettempdir())))
            (root / "home").mkdir()
            origin = f"http://127.0.0.1:{server.server_port}"
            def environment(tag, ws):
                config = ws / ".rustain"; config.mkdir(parents=True)
                (ws / ".env").write_text(""); (config / "config.toml").write_text("[notifications]\ninfo_ttl_secs = 30\n")
                return {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LANG": "C.UTF-8", "TERM": "xterm-256color",
                        "HOME": str(root / "home"), "XDG_CONFIG_HOME": str(root / "config"), "XDG_DATA_HOME": str(root / "xdg-data"),
                        "XDG_CACHE_HOME": str(root / "cache"), "RUSTAIN_CONFIG_DIR": str(config), "RUSTAIN_DATA_DIR": str(ws / "data"),
                        "RUSTAIN_LOG_PATH": str(evidence / f"{tag}.runtime.log"), "RUST_LOG": "debug",
                        "ANTHROPIC_API_KEY": KEY, "ANTHROPIC_BASE_URL": origin, "RUSTAIN_MODELS_DEV_URL": origin, "NO_PROXY": "*"}
            for tag in ("local", "attached"):
                ws = root / tag; env = environment(tag, ws)
                if tag == "attached":
                    daemon_env = {**env, "RUSTAIN_LOG_PATH": str(evidence / "daemon.runtime.log")}
                    _daemon(stack, binary, ws, daemon_env, evidence)
                tui = _Terminal(stack, binary, ["--new"] if tag == "local" else ["daemon", "attach"], ws, env, evidence, tag, errors)
                _leg(tui)
        assert errors.empty(), list(errors.queue)
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
        print(f"NATIVE_21_4_A2A_EVIDENCE={evidence}", flush=True)
