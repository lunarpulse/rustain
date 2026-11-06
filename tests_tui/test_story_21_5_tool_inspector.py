"""Story 21.5 slice A: one 80x24 debug-binary leg; no external provider.

Offline first: RUSTAIN_TOOL_INSPECTOR_DESKCHECK=1 consumes ONLY saved Rust paints
from RUSTAIN_TOOL_INSPECTOR_FRAMES. Export with RUSTAIN_STORY_21_5_FRAME_DIR and
conformance_21_5_tool_inspector::smoke_frames::production_smoke_frames_cover_leg_a_at_every_geometry.
No binary, subprocess, SceneStub or RustainTUI is started in the offline branch.
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
import subprocess
import tempfile
import time
import traceback

import harness
import pexpect
import pytest
from fixtures.scene_provider import SceneStub

SCENE = Path(__file__).parent / "fixtures/scenes/tool_inspector.json"
RAIL = "[Esc] Back  [Left/Right] Tab  [j/k] Scroll"
FULL = "1\n2\n3\n4\n5\n6\n7\n8"
RIGHT = b"\x1b[C"


@pytest.fixture(scope="session", autouse=True)
def build_binary():
    """Override only this module's inherited build: offline means zero spawns."""
    if os.environ.get("RUSTAIN_TOOL_INSPECTOR_DESKCHECK") != "1":
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
    return lines, top-1


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


def _inspector(screen, hidden, tool, active, draft="", anchors=()):
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
    tabs = ["Output", "Input", "Raw"]+(["Diff"] if tool.startswith("Edit") else [])
    expected = "  ".join("["+tab+"]" if tab == active else tab for tab in tabs)
    inside = lambda y: lines[y][left+1:left+bw-1]
    if inside(tabs_y).rstrip() != expected or inside(top+bh-2).rstrip() != RAIL:
        return None
    if bh-2 >= 4 and not re.fullmatch(r"Lines \d+–\d+ of \d+", inside(body_end).strip()):
        return None
    body = tuple(inside(y) for y in range(body_top, body_end))
    def has(anchor):
        return any(line.strip() == anchor if not anchor or anchor.startswith("row ") else anchor in line for line in body)
    if not body or any(not has(anchor) for anchor in anchors) or (not anchors and not any(line.strip() for line in body)):
        return None
    return body


def _stage(screen, hidden, draft="", tool=None):
    shell = _shell(screen, hidden, draft)
    if not shell:
        return None
    lines, status = shell
    stage = tuple(lines[:status])
    if any("Tool · " in line or line.lstrip().startswith("┌─") for line in stage):
        return None
    tools = [match.group(1) for line in stage
             if (match := re.match(r'^│ ┄ (?:✓ Success )?(Bash|Edit|Write) "', line))]
    if tool is not None and (not tools or tools[0] != tool):
        return None
    return stage


def _unobscured_stage(screen, hidden, draft="", tool=None):
    stage = _stage(screen, hidden, draft, tool)
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


def _deskcheck(directory, failed_directory):
    records = [json.loads(path.read_text()) for path in Path(directory).glob("a-*.json")]
    frames = {(r["width"], r["height"], r["full"], r["stage"]): r for r in records}
    assert len(records) == len(frames) and frames, "missing/duplicate real frame exports"
    base_stages = {"completed", "stage", "back", "p-inert", "edit-stage", "edit-back", "ctrl-c-back"}
    outputs = {"output", "left-end", "line-down", "line-up", "half-down", "half-up", "page-down",
               "page-up", "tail", "top", "output-return"}
    inputs = {"input", "input-command", "input-return", "edit-input"}
    raws = {"raw", "raw-end", "raw-tool", "raw-tail", "edit-raw"}
    diffs = {"diff", "diff-tail", "diff-top"}
    stages = base_stages | outputs | inputs | raws | diffs | {"edit-output"}
    expected_keys = set()
    for width, height in [(60, 16), (80, 24), (130, 30)]:
        for full in (False, True):
            labels = stages | {"stage-notice"} | ({"tail-row60"} if (width, height, full) == (60, 16, True) else set())
            for stage in labels:
                expected_keys.add((width, height, full, stage))
                r = frames[width, height, full, stage]
                screen, draft = r["screen"], FULL if full else ""
                assert r["draft"] == draft and r["cursor_hidden"] is True
                assert len(screen.split("\n")) == height and len(screen.split("\n")[0]) == width
                if stage == "stage-notice":
                    assert _shell(screen, True, draft)
                    assert "Info INSPECTOR NOTICE CONTROL" in screen
                    assert not _unobscured_stage(screen, True, draft, "Bash")
                    continue
                if stage == "completed":
                    predicate = lambda s, hidden=True: _completed_reply(s, hidden, draft)
                elif stage in base_stages:
                    expected_tool = "Bash" if stage in {"stage", "back", "p-inert"} else "Edit"
                    predicate = lambda s, hidden=True: _unobscured_stage(s, hidden, draft, expected_tool)
                else:
                    active = ("Output" if stage in outputs or stage in {"edit-output", "tail-row60"} else
                              "Input" if stage in inputs else "Raw" if stage in raws else "Diff")
                    tool = "Edit #2" if stage.startswith("edit-") or stage in diffs else "Bash #1"
                    anchors = ("row 1",) if stage in {"output", "left-end", "top", "output-return", "line-up", "half-up", "page-up"} else ()
                    if stage == "line-down": anchors = ("row 2",)
                    geometry = _geometry(screen, True, draft)
                    viewport = geometry[7]-geometry[6]
                    if stage == "half-down": anchors = (f"row {max(1, viewport//2)+1}",)
                    if stage == "page-down": anchors = (f"row {max(1, viewport-1)+1}",)
                    if stage in inputs: anchors = ("{",)
                    if stage == "input-return": anchors = ('"command"',)
                    if stage in {"raw", "raw-end", "edit-raw"}: anchors = ("{",)
                    if stage == "raw-tail" and viewport > 1: anchors = ("}",)
                    if stage in {"tail", "raw-tail"} and viewport == 1: anchors = ("",)
                    if stage == "edit-output": anchors = ("edited notes.txt",)
                    if stage == "input-command": anchors = ('"command"',)
                    if stage == "raw-tool" or (stage in {"raw", "raw-end"} and width == 80 and not full): anchors = ('"tool": "Bash"',)
                    if stage in {"diff", "diff-top"}: anchors = ("- alpha",)
                    if stage == "diff-tail": anchors = ("+ beta",)
                    if stage == "tail-row60" or (stage == "tail" and not (width == 60 and full)): anchors = ("row 60",)
                    predicate = lambda s, hidden=True: _inspector(s, hidden, tool, active, draft, anchors)
                assert predicate(screen), (stage, screen)
                assert not predicate(screen, False), "visible cursor/unfinished draw rejected"
                # Each independent complete-frame positive must reject shell tears.
                lines = screen.split("\n")
                shell = _shell(screen, True, draft)
                torn_rows = {shell[1], shell[1]+1, height-1}
                if stage not in base_stages:
                    _, left, top, bw, bh, tabs_y, body_top, body_end = _geometry(screen, True, draft)
                    torn_rows |= {top, top+bh-1, tabs_y, top+bh-2}
                    for y in torn_rows:
                        torn = lines.copy(); torn[y] = " "*width
                        assert not predicate("\n".join(torn)), (stage, "torn", y)
                    for a, b in [(top, top+bh//2), (top+bh//2, top+bh)]:
                        torn = lines.copy(); torn[a:b] = [" "*width]*(b-a)
                        assert not predicate("\n".join(torn)), (stage, "half-draw")
                    assert not predicate(screen.replace("Tool · ", "Wrong · ", 1)), stage
                    assert not predicate(screen.replace(RAIL, " "*len(RAIL))), stage
                    wrong = lines.copy()
                    for y in range(body_top, body_end):
                        wrong[y] = wrong[y][:left+1]+"X"*(bw-2)+wrong[y][left+bw-1:]
                    if anchors:
                        assert not predicate("\n".join(wrong)), (stage, "wrong body")
                    for anchor in anchors:
                        if anchor:
                            assert not predicate(screen.replace(anchor, " "*len(anchor))), (stage, anchor)
                else:
                    for y in torn_rows:
                        torn = lines.copy(); torn[y] = " "*width
                        assert not predicate("\n".join(torn)), (stage, y)
                    if stage == "completed":
                        assert not predicate(screen.replace("Inspected three tools.", "Unrelated final reply"))
                        assert not predicate(screen.replace("│ Inspected three tools.", "  Inspected three tools."))
            def saved(stage): return frames[width, height, full, stage]["screen"]
            assert saved("stage") == saved("back") == saved("p-inert")
            assert saved("edit-stage") == saved("edit-back") == saved("ctrl-c-back")
            assert saved("raw") == saved("raw-end")
            assert saved("output") == saved("left-end")
            # Ordered diff rows, including the one-row fitted case's separate views.
            diff = _inspector(saved("diff"), True, "Edit #2", "Diff", FULL if full else "")
            if len(diff) > 1:
                assert diff[0].startswith("- alpha") and diff[1].startswith("+ beta")
    assert set(frames) == expected_keys, set(frames) ^ expected_keys
    # The actual second attempt caught startup overlays in its first baseline.
    # Every saved inspector frame is also a negative for a clean Stage baseline.
    failed = {path.name: json.loads(path.read_text())
              for path in Path(failed_directory).glob("*.frame.json")}
    assert set(failed) == {name+".frame.json" for name in (
        "01-stage", "02-bash-output", "03-bash-tail", "04-bash-input", "05-bash-raw", "06-raw-end")}
    requests = [json.loads(line) for line in
                (Path(failed_directory) / "requests.jsonl").read_text().splitlines()]
    baseline = failed["01-stage.frame.json"]["screen"]
    assert _real_results_ready(baseline, requests)
    assert not _real_results_ready(baseline, [r for r in requests if r.get("turn") != 2])
    missing_results = [dict(r, tool_results=0) if r.get("turn") == 2 else r for r in requests]
    assert not _real_results_ready(baseline, missing_results)
    assert not any(r.get("desync") or r.get("kind") == "unknown" for r in requests)
    assert (Path(failed_directory) / "workspace/notes.txt").read_text() == "beta\n"
    assert (Path(failed_directory) / "workspace/fresh.txt").read_text() == "line one\nline two\n"
    inspector_cases = {
        "02-bash-output.frame.json": ("Output", "row 1"),
        "03-bash-tail.frame.json": ("Output", "row 60"),
        "04-bash-input.frame.json": ("Input", '"command"'),
        "05-bash-raw.frame.json": ("Raw", '"tool": "Bash"'),
        "06-raw-end.frame.json": ("Raw", '"tool": "Bash"'),
    }
    for name, frame in failed.items():
        screen = frame["screen"]
        assert frame["cursor_hidden"] and frame["hidden_draw_complete"]
        assert not _unobscured_stage(screen, True, tool="Bash"), name
        if name == "01-stage.frame.json":
            assert _stage(screen, True, tool="Bash"), "overlay, not a missing Stage"
        else:
            active, anchor = inspector_cases[name]
            assert _inspector(screen, True, "Bash #1", active, anchors=(anchor,)), name
    return len(frames)


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
    manifest = {"started": datetime.now(timezone.utc).isoformat(), "status": "FAIL"}
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
                    "RUSTAIN_PROFILE": "", "DEEPSEEK_API_KEY": "", "STUBAI_API_KEY": ""})
        monkeypatch.setattr(harness, "ENV_FILE", ws / ".env")
        monkeypatch.setattr(harness, "TERM_COLS", 80)
        monkeypatch.setattr(harness, "TERM_ROWS", 24)
        original_spawn = pexpect.spawn
        with (evidence / "terminal.bin").open("wb") as raw:
            def spawn(*args, **kwargs):
                kwargs["encoding"] = None
                child = original_spawn(*args, **kwargs)
                child.logfile_read = raw
                child.delayafterclose = child.delayafterterminate = 0
                return child
            monkeypatch.setattr(harness.pexpect, "spawn", spawn)
            tui = _TUI(build=False, binary=binary, workspace=ws, env_overrides=env)
            tui.prepare_capture(evidence)
            tui.start()
            tui.send_bytes("inspect please", b"inspect please\r")
            # The preserved three-tool flood heuristic folds the completed turn.
            # Wait for actual results/Ready, then use the real unfold front door
            # before requiring the final prose row inside that turn.
            tui.wait_frame(lambda s: _real_results_ready(s, stub.rows()),
                           "three real tool results and Ready", hidden=False)
            assert (ws / "notes.txt").read_text() == "beta\n"
            assert (ws / "fresh.txt").read_text() == "line one\nline two\n"
            tui.send_bytes("Esc to Chat", b"\x1b")
            tui.wait_frame(lambda s: _stage(s, tui.screen.cursor.hidden), "Chat cursor hidden")
            tui.send_bytes("real unfold", b"zR")
            tui.wait_frame(lambda s: _completed_reply(s, tui.screen.cursor.hidden),
                           "unfolded final reply")
            tui.send_bytes("oldest Stage", b"gg")
            before = tui.capture("01-stage", lambda s: _unobscured_stage(s, tui.screen.cursor.hidden, tool="Bash"))
            def inspect(label, keys, tool, active, anchors):
                tui.send_bytes(label, keys)
                return tui.capture(label, lambda s: _inspector(s, tui.screen.cursor.hidden, tool, active, anchors=anchors))
            inspect("02-bash-output", b"\r", "Bash #1", "Output", ("row 1",))
            inspect("03-bash-tail", b"G", "Bash #1", "Output", ("row 60",))
            inspect("04-bash-input", RIGHT, "Bash #1", "Input", ('"command"',))
            raw_screen = inspect("05-bash-raw", RIGHT, "Bash #1", "Raw", ('"tool": "Bash"',))
            assert inspect("06-raw-end", RIGHT, "Bash #1", "Raw", ('"tool": "Bash"',)) == raw_screen
            tui.send_bytes("Back to Stage", b"\x1b")
            returned = tui.capture("07-stage-restored", lambda s: _stage(s, tui.screen.cursor.hidden, tool="Bash") == _stage(before, True, tool="Bash"))
            assert _stage(returned, True) == _stage(before, True)
            tui.send_bytes("p is inert", b"p")
            assert tui.capture("08-p-inert", lambda s: s == returned) == returned
            # Stage j owns the scroll. No Tab or injected focused-tool state.
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
            tui.capture("09-edit-stage", lambda s: _stage(s, tui.screen.cursor.hidden, tool="Edit"))
            inspect("10-edit-output", b"\r", "Edit #2", "Output", ())
            inspect("11-edit-input", RIGHT, "Edit #2", "Input", ('"old_string"',))
            inspect("12-edit-raw", RIGHT, "Edit #2", "Raw", ('"tool": "Edit"',))
            diff = inspect("13-edit-diff", RIGHT, "Edit #2", "Diff", ("- alpha", "+ beta"))
            body = _inspector(diff, True, "Edit #2", "Diff")
            assert body[0].startswith("- alpha") and body[1].startswith("+ beta")
            tui.send_bytes("Back from Edit", b"\x1b")
            tui.capture("14-edit-back", lambda s: _stage(s, tui.screen.cursor.hidden, tool="Edit"))
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


@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(180)
def test_tool_inspector_smoke_leg_a(monkeypatch):
    if os.environ.get("RUSTAIN_TOOL_INSPECTOR_DESKCHECK") == "1":
        def forbidden(*_args, **_kwargs):
            raise AssertionError("inspector DESKCHECK must not start any process")
        monkeypatch.setattr(subprocess, "Popen", forbidden)
        monkeypatch.setattr(pexpect, "spawn", forbidden)
        monkeypatch.setattr(SceneStub, "start", forbidden)
        monkeypatch.setattr(_TUI, "start", forbidden)
        assert _deskcheck(os.environ["RUSTAIN_TOOL_INSPECTOR_FRAMES"],
                          os.environ["RUSTAIN_TOOL_INSPECTOR_FAILED_FRAMES"]) > 0
        return
    _live(monkeypatch)
