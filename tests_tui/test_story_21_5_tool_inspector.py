"""Story 21.5: independent A, B and C launches in one 80x24 debug smoke.

Offline first: RUSTAIN_TOOL_INSPECTOR_DESKCHECK=1 consumes ONLY saved Rust paints
from RUSTAIN_TOOL_INSPECTOR_FRAMES (a/b/c prefixes and matching leg fields).
Export with RUSTAIN_STORY_21_5_FRAME_DIR and the smoke_frames Rust tests.
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
LEGACY_RAIL = "[Esc] Back  [Left/Right] Tab  [j/k] Scroll"
FULL = "1\n2\n3\n4\n5\n6\n7\n8"
OUTPUT = "".join(f"row {n}\n" for n in range(1, 61)).encode()
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


def _notifications(screen, hidden, clipboard, draft=""):
    """Whole S-LOG shell, exact rails and the actual typed clipboard record."""
    geometry = _geometry(screen, hidden, draft)
    if not geometry:
        return None
    lines, left, top, bw, bh, *_ = geometry
    box = [line[left:left+bw] for line in lines[top:top+bh]]
    title = " Tool · Bash #1 › System · Notifications "
    compact = bh <= 6
    secondary = "[j/k] Move [x] Dismiss [/] Search [Esc] Close"
    bottom = secondary if compact else ""
    if (box[0] != "┌"+title+"─"*(bw-len(title)-2)+"┐"
            or box[-1] != "└"+bottom+"─"*(bw-len(bottom)-2)+"┘"
            or any(not (line.startswith("│") and line.endswith("│")) for line in box[1:-1])):
        return None
    inner = tuple(line[1:-1].rstrip() for line in box[1:-1])
    if not re.fullmatch(r"Notifications \([1-9]\d*\)", inner[0]):
        return None
    record = re.match(r"Success · (\d\d:\d\d:\d\d) · clipboard · ", inner[1])
    if not record:
        return None
    expected = record.group(0)+"Copied to "+str(clipboard)
    if len(expected) > bw-2:
        expected = expected[:bw-3]+"…"
    if inner[1] != expected:
        return None
    pairs = (["[Enter] Detail ", "[c] Copy "] if compact else
             ["[j/k] Move ", "[Enter] Detail ", "[c] Copy ",
              "[x] Dismiss ", "[/] Search ", "[Esc] Close"])
    rail = [""]
    for pair in pairs:
        if rail[-1] and len(rail[-1])+len(pair) > bw-2:
            rail.append("")
        rail[-1] += pair
    expected_rail = tuple(line.rstrip() for line in rail)
    if not any(inner[y:y+len(expected_rail)] == expected_rail
               for y in range(2, len(inner)-len(expected_rail)+1)):
        return None
    return inner


def _stage(screen, hidden, draft="", tool=None, *, marked=None, cue=False, profile="current"):
    shell = _shell(screen, hidden, draft)
    if not shell:
        return None
    lines, status = shell
    stage = tuple(lines[:status])
    if any("Tool · " in line or line.lstrip().startswith("┌─") for line in stage):
        return None
    headers = [match for line in stage
               if (match := re.match(r'^(?:▶ )?│ (┄ |┃ |▸ )(?:✓ Success )?(Bash|Edit|Write) "', line))]
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
                assert r["leg"] == "a" and r["draft"] == draft and r["cursor_hidden"] is True
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
                    predicate = lambda s, hidden=True: _unobscured_stage(
                        s, hidden, draft, expected_tool, marked=expected_tool)
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
                    rail = _rail(bw-2, bh-2, tool, active=active)
                    assert not predicate(screen.replace(rail, " "*len(rail))), stage
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
            assert _stage(screen, True, tool="Bash", profile="slice1"), "overlay, not a missing Stage"
        else:
            active, anchor = inspector_cases[name]
            assert _inspector(screen, True, "Bash #1", active, anchors=(anchor,), profile="slice1"), name
            assert not _inspector(screen, True, "Bash #1", active, anchors=(anchor,)), name
    assert len(frames) == 193
    return len(frames)+_deskcheck_bc(directory)


def _reject_torn(screen, draft, predicate, label, inspector=True):
    """Use complete real positives; mutate independent shell/frame boundaries."""
    assert predicate(screen), (label, screen)
    assert not predicate(screen, False), (label, "visible cursor")
    lines = screen.split("\n")
    width, height = len(lines[0]), len(lines)
    shell = _shell(screen, True, draft)
    rows = {shell[1], shell[1]+1, height-1}
    if inspector == "notifications":
        _, left, top, bw, bh, *_ = _geometry(screen, True, draft)
        rows |= {top, top+bh-1, top+1, top+2}
        rows |= {y for y in range(top+1, top+bh-1)
                 if any(key in lines[y][left:left+bw] for key in ("[Enter]", "[j/k]", "[Esc]"))}
        assert not predicate(screen.replace("System · Notifications", "System · Wrong notices")), label
        for start, end in [(top, top+bh//2), (top+bh//2, top+bh)]:
            torn = lines.copy()
            torn[start:end] = [" "*width]*(end-start)
            assert not predicate("\n".join(torn)), (label, "half draw")
    elif inspector:
        _, left, top, bw, bh, tabs_y, body_top, body_end = _geometry(screen, True, draft)
        rows |= {top, top+bh-1, tabs_y, top+bh-2}
        if bh-2 >= 4:
            rows.add(body_end)
        if bh-2 >= 5:
            rows.add(top+1)
        assert not predicate(screen.replace("Tool · ", "Wrong · ", 1)), label
        for start, end in [(top, top+bh//2), (top+bh//2, top+bh)]:
            torn = lines.copy()
            torn[start:end] = [" "*width]*(end-start)
            assert not predicate("\n".join(torn)), (label, "half draw")
        wrong = lines.copy()
        for y in range(body_top, body_end):
            wrong[y] = wrong[y][:left+1]+"X"*(bw-2)+wrong[y][left+bw-1:]
        assert not predicate("\n".join(wrong)), (label, "wrong body")
    for y in rows:
        torn = lines.copy()
        torn[y] = " "*width
        assert not predicate("\n".join(torn)), (label, "torn", y)


def _deskcheck_bc(directory):
    b_stages = {
        "stage", "tab-edit", "edit-output", "write-output", "write-diff",
        "write-boundary", "edit-diff", "bash-output", "bash-boundary", "back",
        "offscreen-stage", "offscreen-tab", "offscreen-back",
    }
    c_stages = {
        "stage", "output", "query-field", "search1", "search2", "wrap-off",
        "copied", "cleared", "back", "control-stage", "split-output", "split-field",
        "split-match", "split-next", "split-no-match", "split-clear",
        "split-wrap-off", "split-wrap-on", "changed-source", "control-back",
        "copy-notifications", "copy-return",
    }
    frames = {}
    for leg in ("b", "c"):
        paths = list(Path(directory).glob(leg+"-*.json"))
        for path in paths:
            r = json.loads(path.read_text())
            key = (leg, r["width"], r["height"], r["full"], r["stage"])
            assert key not in frames and r["leg"] == leg, path
            frames[key] = r
    expected = set()
    for width, height in [(60, 16), (80, 24), (130, 30)]:
        for full in (False, True):
            draft = FULL if full else ""
            def saved(leg, stage):
                return frames[leg, width, height, full, stage]["screen"]
            for leg, stages in (("b", b_stages), ("c", c_stages)):
                for stage in stages:
                    key = (leg, width, height, full, stage)
                    expected.add(key)
                    r = frames[key]
                    screen = r["screen"]
                    assert r["draft"] == draft and r["cursor_hidden"] is True
                    assert len(screen.split("\n")) == height
                    assert all(len(line) == width for line in screen.split("\n"))
                    is_stage = stage in {
                        "stage", "tab-edit", "back", "offscreen-stage",
                        "offscreen-tab", "offscreen-back", "control-stage", "control-back",
                    }
                    if stage == "copy-notifications":
                        clipboard = Path(directory) / f"c-{width}x{height}-{'full' if full else 'empty'}-clipboard.txt"
                        predicate = lambda s, hidden=True: _notifications(s, hidden, clipboard, draft)
                    elif is_stage:
                        marked = ("Edit" if leg == "b" and stage in {
                            "tab-edit", "back", "offscreen-tab", "offscreen-back",
                        } else "Bash")
                        predicate = lambda s, hidden=True: _unobscured_stage(
                            s, hidden, draft, marked=marked, cue=leg == "b" and width == 80 and marked == "Bash")
                    else:
                        tool, active, anchors = "Bash #1", "Output", ()
                        options = {}
                        if leg == "b":
                            if stage in {"edit-output", "edit-diff"}:
                                tool = "Edit #2"
                            elif stage.startswith("write-"):
                                tool = "Write #3"
                            if stage in {"write-diff", "write-boundary", "edit-diff"}:
                                active = "Diff"
                            anchors = {
                                "edit-output": ("edited notes.txt",),
                                "write-output": ("wrote fresh.txt",),
                                "write-diff": ("+ line one",),
                                "write-boundary": ("+ line one",),
                                "edit-diff": ("- alpha",),
                                "bash-output": ("row 1",),
                                "bash-boundary": ("row 1",),
                            }[stage]
                        else:
                            options, anchors = _c_expectation(stage, width, height, full)
                        predicate = lambda s, hidden=True: _inspector(
                            s, hidden, tool, active, draft, anchors, **options)
                    frame_kind = "notifications" if stage == "copy-notifications" else not is_stage
                    _reject_torn(screen, draft, predicate, key, frame_kind)
                    if is_stage:
                        assert not predicate(screen.replace("▸ ", "┄ ")), (key, "missing marker")
                        if leg == "b" and width == 80 and marked == "Bash":
                            assert not predicate(screen.replace("[Enter] Inspect", " "*15)), (key, "missing cue")
            assert saved("b", "tab-edit") == saved("b", "back")
            assert saved("b", "write-diff") == saved("b", "write-boundary")
            assert saved("b", "bash-output") == saved("b", "bash-boundary")
            assert saved("b", "offscreen-tab") == saved("b", "offscreen-back")
            assert saved("b", "offscreen-stage") != saved("b", "offscreen-tab")
            assert saved("c", "stage") == saved("c", "back")
            assert saved("c", "control-stage") == saved("c", "control-back")
            clipboard = Path(directory) / f"c-{width}x{height}-{'full' if full else 'empty'}-clipboard.txt"
            assert clipboard.read_bytes() == OUTPUT
            if width < 100:
                assert saved("c", "wrap-off") == saved("c", "copied") == saved("c", "copy-return")
            assert saved("c", "split-next") != saved("c", "split-no-match")
            assert saved("c", "split-output") != saved("c", "changed-source")
    assert set(frames) == expected, set(frames) ^ expected
    assert len(frames) == 210
    return len(frames)


def _c_expectation(stage, width, height, full):
    options = {}
    anchors = ()
    if stage in {"output", "query-field"}:
        options["first"] = "row 1"
    if stage == "query-field":
        options["field"] = "row 4"
    if stage in {"search1", "search2", "wrap-off", "copied", "copy-return"}:
        options["search"] = "Search 1/11" if stage == "search1" else "Search 2/11"
        options["first"] = "row 4" if stage == "search1" else "row 40"
    if stage in {"wrap-off", "copied", "copy-return", "cleared"}:
        options["wrap"] = False
    if stage == "cleared":
        options["first"] = "row 40"
    # The companion Rust control is a real call whose word spans a soft wrap.
    if stage.startswith("split-") or stage == "changed-source":
        body_width = width-2 if width < 100 else width*9//10-2
        first = "x"*(body_width-2)+"AL"
        if stage in {"split-output", "split-field", "split-match", "split-wrap-on"}:
            options["first"] = first
        if stage == "split-field":
            options["field"] = "alpha"
        if stage in {"split-match", "split-next"}:
            options["search"] = "Search 1/1"
        if stage in {"split-next", "split-no-match", "split-clear"}:
            options["first"] = "PHA"
        if stage == "split-no-match":
            options["search"] = 'Search: no match for "missing"'
        if stage == "split-wrap-off":
            options.update(wrap=False, first="x"*(body_width-2)+"A→")
        if stage == "changed-source":
            options["first"] = "changed source"
    return options, anchors


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


def _live(monkeypatch, leg="a"):
    scratch = Path(os.environ.get("TMPDIR", tempfile.gettempdir())).resolve()
    assert scratch.is_dir(), scratch
    evidence_parent = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", scratch / "rustain-21-5-evidence")).resolve()
    evidence_parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix=leg+"-", dir=evidence_parent))
    manifest = {"leg": leg, "started": datetime.now(timezone.utc).isoformat(), "status": "FAIL"}
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
                    s, tui.screen.cursor.hidden, tool="Bash", marked="Bash", cue=leg == "b"))
                if leg == "a":
                    _leg_a(tui, before)
                elif leg == "b":
                    _leg_b(tui, before)
                else:
                    assert leg == "c"
                    _leg_c(tui, before, ws, manifest)
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


def _leg_b(tui, before):
    assert _unobscured_stage(before, True, marked="Bash", cue=True)
    tui.send_bytes("real Tab to Edit", b"\t")
    edit_before = tui.capture("02-tab-edit", lambda s: _unobscured_stage(
        s, tui.screen.cursor.hidden, marked="Edit"))
    assert edit_before != before
    _inspect(tui, "03-edit-output", b"\r", "Edit #2", "Output", ("Successfully wrote 5 bytes to notes.txt",))
    _inspect(tui, "04-write-output", b"]", "Write #3", "Output")
    _inspect(tui, "05-write-diff", RIGHT*3, "Write #3", "Diff", ("+ line one", "+ line two"))
    write = "\n".join(tui.screen.display)
    assert _inspect(tui, "06-write-boundary", b"]", "Write #3", "Diff", ("+ line one",)) == write
    _inspect(tui, "07-edit-diff", b"[", "Edit #2", "Diff", ("- alpha", "+ beta"))
    bash = _inspect(tui, "08-bash-output", b"[", "Bash #1", "Output", first="row 1")
    assert _inspect(tui, "09-bash-boundary", b"[", "Bash #1", "Output", first="row 1") == bash
    tui.send_bytes("Back preserves real Tab Stage focus", b"\x1b")
    assert tui.capture("10-stage-restored", lambda s: s == edit_before
                       and _unobscured_stage(s, tui.screen.cursor.hidden, marked="Edit")) == edit_before


def _leg_c(tui, before, workspace, manifest):
    """Approved legacy-preserving notice and two-Escape child-field route."""
    _inspect(tui, "02-bash-output", b"\r", "Bash #1", "Output", first="row 1")
    _inspect(tui, "03-query-field", b"/row 4", "Bash #1", "Output",
             field="row 4", first="row 1")
    _inspect(tui, "04-search-first", b"\r", "Bash #1", "Output",
             search="Search 1/11", first="row 4")
    _inspect(tui, "05-search-next", b"n", "Bash #1", "Output",
             search="Search 2/11", first="row 40")
    sheet = _inspect(tui, "06-wrap-off", b"w", "Bash #1", "Output",
                     search="Search 2/11", wrap=False, first="row 40")
    # harness.start uses this explicit portable data-dir override. Never parse a
    # clipped toast/log pathname, and never substitute painted/selected rows.
    clipboard = workspace / ".rustain_data/clipboard.txt"
    assert not clipboard.exists()
    tui.send_bytes("copy actual source", b"c")
    copied = tui.capture("07-copied-sheet", lambda s: s == sheet
                         and _inspector(s, tui.screen.cursor.hidden, "Bash #1", "Output",
                                        search="Search 2/11", wrap=False, first="row 40")
                         and clipboard.is_file() and clipboard.read_bytes() == OUTPUT)
    assert copied == sheet and clipboard.read_bytes() == OUTPUT
    manifest.update(
        clipboard={"path": ".rustain_data/clipboard.txt", "bytes": len(OUTPUT),
                   "sha256": hashlib.sha256(clipboard.read_bytes()).hexdigest()},
        c_route={
            "authority": "Main approved 2026-11-06",
            "literal_plan": "c -> Copied toast; Esc Esc -> initial Stage",
            "preserved_policy": "toasts below opaque frames; committed search is not a child editor",
            "actual_keys": "c; Ctrl+X,n; Esc; /; Esc; Esc",
            "notice_evidence": "actual System Notifications clipboard success record, not a sheet toast",
        },
    )
    tui.send_bytes("real Notifications over inspector", b"\x18n")
    tui.capture("08-copy-notifications", lambda s: _notifications(
        s, tui.screen.cursor.hidden, clipboard))
    tui.send_bytes("Back from Notifications", b"\x1b")
    assert tui.capture("09-copy-return", lambda s: s == sheet
                       and _inspector(s, tui.screen.cursor.hidden, "Bash #1", "Output",
                                      search="Search 2/11", wrap=False, first="row 40")) == sheet
    _inspect(tui, "10-reopen-query", b"/", "Bash #1", "Output",
             field="row 4", search="Search 2/11", wrap=False, first="row 40")
    _inspect(tui, "11-clear-query", b"\x1b", "Bash #1", "Output",
             wrap=False, first="row 40")
    tui.send_bytes("Back from inspector", b"\x1b")
    assert tui.capture("12-stage-restored", lambda s: s == before
                       and _unobscured_stage(s, tui.screen.cursor.hidden, marked="Bash")) == before


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
    """Historical node name is retained for the subprocess-free resolver callers."""
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
    for leg in ("a", "b", "c"):
        _live(monkeypatch, leg)
