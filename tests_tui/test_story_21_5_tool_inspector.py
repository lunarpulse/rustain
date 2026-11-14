"""Story 21.5: the A launch of the tool inspector, one 80x24 debug smoke.

Live evidence (including failures) stays in RUSTAIN_CAPTURE_EVIDENCE_DIR; workspace
sockets use TMPDIR, which must be short enough. No fallback directory or retries.
"""
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import tempfile
import time
import traceback

import harness
import pexpect
import pytest
from fixtures.scene_provider import SceneStub

SCENE = Path(__file__).parent / "fixtures/scenes/tool_inspector.json"
LEGACY_RAIL = "[Esc] Back  [Left/Right] Tab  [j/k] Scroll"
RIGHT = b"\x1b[C"


@pytest.fixture(scope="session", autouse=True)
def build_binary():
    """Assert the supplied or resolved binary exists (this module's own override of the inherited build)."""
    assert harness._resolve_binary()[0].is_file()


def _shell(screen, hidden, draft=""):
    lines = screen.split("\n")
    if (not hidden or len(lines) not in (16, 24, 30) or len(lines[0]) not in (60, 80, 130)
            or any(len(line) != len(lines[0]) for line in lines)):
        return None
    top = harness.composer_top(lines)
    width = len(lines[0])
    titles = (" Message ", " Message [ML] ")
    if (top is None or top < 2 or not any(lines[top] == "┌"+title+"─"*(width-len(title)-2)+"┐" for title in titles)
            or not re.search(r"\bReady\b", lines[top-1])
            or lines[-1] != "└"+"─"*(width-2)+"┘"
            or any(not (line.startswith("│") and line.endswith("│")) for line in lines[top+1:-1])
            or [line[1:-1].strip() for line in lines[top+1:-1]] != (draft.split("\n") if draft else [""])):
        return None
    # Story 21.7: the Stage region ends above the mission strip row when one is painted.
    return lines, top-1-(1 if harness.strip_row(screen) else 0)


def _geometry(screen, hidden, draft=""):
    shell = _shell(screen, hidden, draft)
    if not shell:
        return None
    lines, region = shell
    width = len(lines[0])
    bw, bh = (width, region) if width < 100 else (width*9//10, min(region, max(5, region*85//100)))
    left, top = (width-bw)//2, (region-bh)//2
    inner = bh-2
    identity, status = int(inner >= 5), int(inner >= 4)
    tabs = top+1+identity
    body_top, body_end = tabs+1, top+bh-2-status
    return lines, left, top, bw, bh, tabs, body_top, body_end


def _rail(width, inner_height, tool, wrap=True, active="Output"):
    """Exact registry-derived action order, fitted as the production rail is.

    21-13 (AC12): the Output tab is a Markdown body, so it offers `v` right after
    the tab pair; the other tabs do not.
    """
    label = "[Esc] Back"
    if inner_height < 4:
        label += f"  [w] Wrap {'on' if wrap else 'off'}  [c] Copy"
    label += "  [Left/Right] Tab"
    if active == "Output":
        label += "  [v] Rendered"
    label += "  [j/k] Scroll  [/] Search"
    ordinal = int(tool.rsplit("#", 1)[1])
    if ordinal > 1:
        label += "  [[] Previous"
    if ordinal < 3:
        label += "  []] Next"
    if inner_height == 4:
        label += "  [w] Wrap  [c] Copy"
    return label if len(label) <= width else label[:width-1]+"…"


def _inspector(screen, hidden, tool, active, draft="", anchors=(), *,
               search=None, field=None, wrap=True, first=None, profile="current"):
    geometry = _geometry(screen, hidden, draft)
    if not geometry:
        return None
    lines, left, top, bw, bh, tabs_y, body_top, body_end = geometry
    box = [line[left:left+bw] for line in lines[top:top+bh]]
    title = "Tool · "+tool
    if (box[0] != "┌"+title+"─"*(bw-len(title)-2)+"┐"
            or box[-1] != "└"+"─"*(bw-2)+"┘"
            or any(not (line.startswith("│") and line.endswith("│")) for line in box[1:-1])):
        return None
    tabs = ["Output", "Input", "Raw"]+(["Diff"] if tool.startswith(("Edit", "Write")) else [])
    expected = "  ".join("["+tab+"]" if tab == active else tab for tab in tabs)
    inside = lambda y: lines[y][left+1:left+bw-1]
    inner_width, inner_height = bw-2, bh-2
    compact = inner_height < 4
    if field is not None or (compact and search is not None):
        field_width = inner_width-(min(len(expected), max(0, inner_width-12))+2)
        tabs_width = inner_width-field_width-2
        query = "/ "+field if field is not None else search
        expected = expected[:tabs_width].ljust(tabs_width+2)+query[:field_width]
    rail = (LEGACY_RAIL if profile == "slice1"
            else _rail(inner_width, inner_height, tool, wrap, active))
    if inside(tabs_y).rstrip() != expected.rstrip() or inside(top+bh-2).rstrip() != rail:
        return None
    if not compact:
        status = inside(body_end).rstrip()
        prefix = "" if profile == "slice1" else f"Wrap {'on' if wrap else 'off'} · "
        if search is not None:
            prefix = search+" · "+prefix
        # Long no-match status is clipped by the real painter, not normalized.
        if len(prefix) >= inner_width:
            if status != prefix[:inner_width]:
                return None
        else:
            suffix = status[len(prefix):] if status.startswith(prefix) else ""
            complete = re.fullmatch(r"Lines \d+–\d+ of \d+", suffix)
            clipped = (len(status) == inner_width and
                       re.fullmatch(r"Lines \d+–\d+(?: of \d*)?", suffix))
            if not (complete or clipped):
                return None
    if inner_height >= 5 and profile == "current":
        if not inside(top+1).startswith("[w] Wrap  [c] Copy · tc_"):
            return None
    body = tuple(inside(y) for y in range(body_top, body_end))
    def has(anchor):
        return any(line.strip() == anchor if not anchor or anchor.startswith("row ") else anchor in line for line in body)
    if (not body or any(not has(anchor) for anchor in anchors)
            or (not anchors and not any(line.strip() for line in body))
            or (first is not None and body[0].rstrip() != first)):
        return None
    return body


def _stage(screen, hidden, draft="", tool=None, *, marked=None, cue=False, profile="current"):
    shell = _shell(screen, hidden, draft)
    if not shell:
        return None
    lines, status = shell
    stage = tuple(lines[:status])
    if any("Tool · " in line or line.lstrip().startswith("┌─") for line in stage):
        return None
    headers = [match for line in stage
               if (match := re.match(r'^(?:▶ )?│ (┄ |┃ |▸ )(?:[●✓✗⊘] )?(Bash|Edit|Write) ', line))]
    if tool is not None and (not headers or headers[0].group(2) != tool):
        return None
    markers = [match for match in headers if match.group(1) == "▸ "]
    if marked is not None and (len(markers) != 1 or markers[0].group(2) != marked):
        return None
    if cue and (len(markers) != 1 or not markers[0].string.rstrip().endswith("[Enter] Inspect")):
        return None
    if profile == "slice1" and markers:
        return None
    return stage


def _unobscured_stage(screen, hidden, draft="", tool=None, **kwargs):
    stage = _stage(screen, hidden, draft, tool, **kwargs)
    # toast_layer paints a severity word over Stage pixels; a compact tool's
    # checked success/error header is not a toast. Never strip overlay pixels
    # from the comparison: wait for their real expiry before taking the baseline.
    notice = r"(?<![✓✗] )\b(?:Info|Success|Warning|Error) "
    return stage if stage and not any(re.search(notice, line) for line in stage) else None


def _real_results_ready(screen, requests):
    turns = [r for r in requests if "turn" in r]
    return (_shell(screen, True) and len(turns) == 2
            and turns[1]["tool_results"] == 3)


def _completed_reply(screen, hidden, draft=""):
    stage = _stage(screen, hidden, draft)
    return stage if stage and any(
        line.rstrip() == "│ Inspected three tools." for line in stage
    ) else None


class _TUI(harness.RustainTUI):
    """Reuse harness launch/config/public child API, but never its sleep helpers."""
    def prepare_capture(self, evidence):
        self.evidence = evidence
        self.screen = harness.TerminalScreen(80, 24)
        self.stream = harness.pyte.ByteStream(self.screen)
        self.draw_tail = b""
        self.draw_complete = False
        self.sent = []

    def read(self):
        try:
            data = self.child.read_nonblocking(65536, timeout=0.1)
        except pexpect.TIMEOUT:
            assert self.child.isalive(), "TUI exited unexpectedly"
            return False
        self.stream.feed(data)
        self.draw_tail = (self.draw_tail+data)[-6:]
        self.draw_complete = self.draw_tail == b"\x1b[?25l"
        with (self.evidence / "screens.jsonl").open("a") as log:
            log.write(json.dumps({"at": time.monotonic(), "screen": "\n".join(self.screen.display),
                                  "cursor_hidden": self.screen.cursor.hidden,
                                  "hidden_draw_complete": self.draw_complete})+"\n")
        return True

    def wait_frame(self, predicate, label, hidden=True, timeout=45):
        deadline = time.monotonic()+timeout
        while time.monotonic() < deadline:
            changed = self.read()
            screen = "\n".join(self.screen.display)
            # A completed postlude plus a quiet read rejects partial PTY draws,
            # while still permitting a genuinely inert key to produce no redraw.
            if (not changed and predicate(screen) and self.screen.cursor.hidden == hidden
                    and (not hidden or self.draw_complete)):
                return screen
        raise AssertionError(f"{label}: no settled complete frame\n"+"\n".join(self.screen.display))

    def wait_for_screen(self, text, timeout=15, poll_interval=0.5):
        # Called by the unchanged harness start(); successful return avoids its
        # legacy fallback sleep. Input focus intentionally has a visible cursor.
        self.wait_frame(lambda s: _shell(s, True) is not None, "startup Ready shell", hidden=False, timeout=timeout)
        return True

    def send_bytes(self, label, data):
        self.sent.append({"at": time.monotonic(), "label": label, "hex": data.hex()})
        (self.evidence / "sent.json").write_text(json.dumps(self.sent, indent=2)+"\n")
        assert os.write(self.child.child_fd, data) == len(data)

    def capture(self, label, predicate):
        screen = self.wait_frame(predicate, label)
        (self.evidence / (label+".screen.txt")).write_text(screen+"\n")
        (self.evidence / (label+".frame.json")).write_text(json.dumps({
            "screen": screen, "cursor_hidden": self.screen.cursor.hidden,
            "hidden_draw_complete": self.draw_complete})+"\n")
        return screen

    def stop(self):
        try:
            child = self.child
        except RuntimeError:
            return
        child.logfile_read = None
        child.close(force=True)


def _live(monkeypatch):
    scratch = Path(os.environ.get("TMPDIR", tempfile.gettempdir())).resolve()
    assert scratch.is_dir(), scratch
    evidence_parent = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", scratch / "rustain-21-5-evidence")).resolve()
    evidence_parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="a-", dir=evidence_parent))
    manifest = {"leg": "a", "started": datetime.now(timezone.utc).isoformat(), "status": "FAIL"}
    ws = None
    stub = None
    tui = None
    try:
        binary = harness._resolve_binary()[0].resolve(strict=True)
        assert binary.is_file() and binary.parent.name == "debug", "debug binary required"
        manifest.update(binary=str(binary), binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest())
        ws = Path(tempfile.mkdtemp(prefix="i-", dir=scratch))
        # Never silently fall back to /tmp when the scoped TMPDIR is too long.
        assert len(os.fsencode(ws / ".rustain_data/daemons" / ("0"*16+".sock"))) < 108, "shorten TMPDIR for AF_UNIX"
        home = ws / "home"; home.mkdir()
        (ws / ".env").write_text("")
        (ws / ".rustain").mkdir()
        (ws / ".rustain/config.toml").write_text('[permissions]\nalways_tools = ["Bash", "Edit", "Write"]\n')
        (ws / "notes.txt").write_text("alpha\n")
        stub = SceneStub(SCENE, evidence / "requests.jsonl").start()
        manifest.update(scene_sha256=stub.sha256, workspace=str(ws), viewport=[80,24])
        env = stub.env("scene-tool-inspector", home)
        env.update({"XDG_CONFIG_HOME": str(home / "config"), "XDG_DATA_HOME": str(home / "data"),
                    "RUSTAIN_LOG_PATH": str(ws / "runtime.log"), "RUST_LOG": "debug",
                    "RUSTAIN_CONFIG_DIR": str(ws / ".rustain"),
                    "RUSTAIN_DATA_DIR": str(ws / ".rustain_data"),
                    "RUSTAIN_CLIPBOARD": "file",
                    "RUSTAIN_PROFILE": "", "DEEPSEEK_API_KEY": "", "STUBAI_API_KEY": ""})
        original_spawn = pexpect.spawn
        # Restore the real spawn BEFORE closing this leg's raw log. No wrapper
        # may capture an earlier launch's closed file in the next fresh leg.
        with (evidence / "terminal.bin").open("wb") as raw, monkeypatch.context() as patch:
            patch.setattr(harness, "ENV_FILE", ws / ".env")
            patch.setattr(harness, "TERM_COLS", 80)
            patch.setattr(harness, "TERM_ROWS", 24)
            def spawn(*args, **kwargs):
                kwargs["encoding"] = None
                child = original_spawn(*args, **kwargs)
                child.logfile_read = raw
                child.delayafterclose = child.delayafterterminate = 0
                return child
            patch.setattr(harness.pexpect, "spawn", spawn)
            tui = _TUI(build=False, binary=binary, workspace=ws, env_overrides=env)
            tui.prepare_capture(evidence)
            try:
                tui.start()
                tui.send_bytes("inspect please", b"inspect please\r")
                # Actual three-result follow-up and Ready precede real zR;
                # a default-folded turn cannot yet expose its final prose.
                tui.wait_frame(lambda s: _real_results_ready(s, stub.rows()),
                               "three real tool results and Ready", hidden=False)
                assert (ws / "notes.txt").read_bytes() == b"beta\n"
                assert (ws / "fresh.txt").read_bytes() == b"line one\nline two\n"
                tui.send_bytes("Esc to Chat", b"\x1b")
                tui.wait_frame(lambda s: _stage(s, tui.screen.cursor.hidden), "Chat cursor hidden")
                tui.send_bytes("real unfold", b"zR")
                tui.wait_frame(lambda s: _completed_reply(s, tui.screen.cursor.hidden),
                               "unfolded final reply")
                tui.send_bytes("oldest Stage", b"gg")
                before = tui.capture("01-stage", lambda s: _unobscured_stage(
                    s, tui.screen.cursor.hidden, tool="Bash", marked="Bash"))
                _leg_a(tui, before)
                _quit(tui)
                manifest.update(exitstatus=tui.child.exitstatus, signalstatus=tui.child.signalstatus,
                                captures=sorted(path.name for path in evidence.glob("*.frame.json")))
            finally:
                tui.stop()
        requests = stub.rows()
        turns = [r for r in requests if "turn" in r]
        assert len(turns) == 2 and turns[1]["tool_results"] == 3, requests
        assert not any(r.get("desync") or r.get("kind") == "unknown" for r in requests), requests
        manifest["status"] = "PASS"
    except BaseException:
        manifest["exception"] = traceback.format_exc()
        raise
    finally:
        cleanup_errors = []
        def sockets_only(root, names):
            sockets = [name for name in names if (Path(root) / name).is_socket()]
            manifest.setdefault("uncopied_unix_sockets", []).extend(str(Path(root) / name) for name in sockets)
            return sockets
        for label, cleanup in [
            ("terminal", tui.stop if tui is not None else None),
            ("stub", stub.stop if stub is not None else None),
            ("workspace", (lambda: shutil.copytree(ws, evidence / "workspace", ignore=sockets_only)) if ws is not None else None),
        ]:
            if cleanup is not None:
                try:
                    result = cleanup()
                    if label == "stub":
                        manifest["stub_exit"] = result
                        assert result == 0, f"SceneStub exited {result}"
                except BaseException:
                    cleanup_errors.append({"phase": label, "exception": traceback.format_exc()})
        if cleanup_errors:
            manifest.update(status="FAIL", cleanup_errors=cleanup_errors)
        manifest["finished"] = datetime.now(timezone.utc).isoformat()
        (evidence / "run.json").write_text(json.dumps(manifest, indent=2)+"\n")
    assert not cleanup_errors, manifest


def _inspect(tui, label, keys, tool, active, anchors=(), **options):
    tui.send_bytes(label, keys)
    return tui.capture(label, lambda s: _inspector(
        s, tui.screen.cursor.hidden, tool, active, anchors=anchors, **options))


def _leg_a(tui, before):
    """Retain all fourteen original A captures, navigation and end guards."""
    _inspect(tui, "02-bash-output", b"\r", "Bash #1", "Output", ("row 1",))
    _inspect(tui, "03-bash-tail", b"G", "Bash #1", "Output", ("row 60",))
    _inspect(tui, "04-bash-input", RIGHT, "Bash #1", "Input", ('"command"',))
    raw_screen = _inspect(tui, "05-bash-raw", RIGHT, "Bash #1", "Raw", ('"tool": "Bash"',))
    assert _inspect(tui, "06-raw-end", RIGHT, "Bash #1", "Raw", ('"tool": "Bash"',)) == raw_screen
    tui.send_bytes("Back to Stage", b"\x1b")
    returned = tui.capture("07-stage-restored", lambda s: s == before
                           and _unobscured_stage(s, tui.screen.cursor.hidden, tool="Bash"))
    assert returned == before
    tui.send_bytes("p is inert", b"p")
    assert tui.capture("08-p-inert", lambda s: s == returned) == returned
    # A keeps real Stage j navigation, never Tab or injected focused-tool state.
    screen = returned
    for _ in range(100):
        previous = _stage(screen, True)
        tui.send_bytes("Stage j toward Edit", b"j")
        screen = tui.wait_frame(lambda s: _stage(s, tui.screen.cursor.hidden)
                                and _stage(s, True) != previous, "settled changed Stage j")
        if _stage(screen, True, tool="Edit"):
            break
    else:
        raise AssertionError("Stage j never made Edit the first visible tool")
    edit_before = tui.capture("09-edit-stage", lambda s: _unobscured_stage(
        s, tui.screen.cursor.hidden, tool="Edit", marked="Edit"))
    _inspect(tui, "10-edit-output", b"\r", "Edit #2", "Output", ("Successfully wrote 5 bytes to notes.txt",))
    _inspect(tui, "11-edit-input", RIGHT, "Edit #2", "Input", ('"old_string"',))
    _inspect(tui, "12-edit-raw", RIGHT, "Edit #2", "Raw", ('"tool": "Edit"',))
    diff = _inspect(tui, "13-edit-diff", RIGHT, "Edit #2", "Diff", ("- alpha", "+ beta"))
    body = _inspector(diff, True, "Edit #2", "Diff")
    assert body[0].startswith("- alpha") and body[1].startswith("+ beta")
    tui.send_bytes("Back from Edit", b"\x1b")
    assert tui.capture("14-edit-back", lambda s: s == edit_before
                       and _unobscured_stage(s, tui.screen.cursor.hidden, tool="Edit")) == edit_before


def _quit(tui):
    tui.send_bytes("explicit quit", b"\x18\x11")
    deadline = time.monotonic()+15
    while time.monotonic() < deadline:
        try:
            tui.read()
        except pexpect.EOF:
            tui.child.close()
            break
    else:
        raise AssertionError("explicit quit did not exit")
    assert (tui.child.exitstatus, tui.child.signalstatus) == (0, None)


@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(180)
def test_tool_inspector_smoke_leg_a(monkeypatch):
    """Leg a: one launch of the real debug binary (the node name is historical)."""
    _live(monkeypatch)
