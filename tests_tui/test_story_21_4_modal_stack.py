"""21.4 smoke legs A1/A2a/A2b/Ba/Bb1a/Bb2a and local Bb1b-i/ii; debug binary.

No API/new marker, sleeps, imported driving helpers or canonical receipts. A
held loopback model turn crosses a tiny budget over scrolled Help. Unknown local
slash commands / attached-local refusals create notices; a draft guards quit.
Structural waits require the whole box, footer, status and cursor, not a title
substring. Raw sent/received bytes, settled screens, dated logs and binary SHA
stay in an env-scoped scratch run; publication is the parent's responsibility.
RUSTAIN_MODAL_STACK_DESKCHECK=1 exercises predicates OFFLINE (no binary/spawn).
The independent Bb1b-i node runs the local Bookmark/Wave leg in the generic lane too.
With DESKCHECK=1, RUSTAIN_MODAL_STACK_BB1B_I_FRAMES must name the Rust export
directory (120 bb1b-i-*.json frames); no prior-leg evidence is needed.
Export before capture under gate-4bb1b-i; capture under smoke-evidence-4bb1b-i.
The generic lane is not dedicated evidence, but a design failure there counts
as the first execution: quarantine it unchanged, then exactly one corrected capture.
Bb1b-ii independently consumes RUSTAIN_MODAL_STACK_BB1B_II_FRAMES offline;
its native leg uses one real local turn, supported History, then live resize.
Bb2a extends the main node: palette caret/paste in both hosts, local Search and
ReverseSearch. Its 102 real-frame controls use RUSTAIN_MODAL_STACK_BB2A_FRAMES.
Bb2b adds the six-field completion leg: shared Log/Help InspectorSearch in both
hosts and real local CrossSearch pasted-query scans. Predicates consume only
complete Rust exports through RUSTAIN_MODAL_STACK_BB2B_FRAMES before capture.
Live inspector captures also require Crossterm's complete hidden-cursor draw
postlude (including split CSI reads), not merely intact borders; each Help
scroll is independently observed before its exact restoration is compared.
C is an independent local/writer/read-only leg, captured under smoke-evidence-4c.
Its zero-spawn DESKCHECK branch requires RUSTAIN_MODAL_STACK_C_FRAMES: c-*.json
production exports at 60x16/80x24/130x30, indexed by width/height/host/stage.
Collection is not dedicated evidence; a design failure still quarantines that
single attempt unchanged. Only the parent runs the scrubbed dedicated capture.
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
FULL_DRAFT = "1\n2\n3\n4\n5\n6\n7\n8"
LOG = "System · Notifications"
QUIT = "System · Notifications › System · Quit"
HELP = "Help — Rustain Keybindings"
HELP_LOG = HELP+" › System · Notifications"
HELP_QUIT = HELP+" › System · Quit"
BUDGET = HELP+" › Decision: Daily budget"
BUDGET_RAIL = ("[y] Continue", "[s] Switch model", "[p] Pause until tomorrow", "[Esc] Later")
NO_DECISIONS = "No decisions waiting"
TURN = "A2b-held-budget"
REPLY = "A2b budget completion"
# Markdown hard breaks keep sixty real rows at every width; the inherited A2b
# completion marker remains last and fits in its bottom-of-Stage predicate.
SEARCH_REPLY = "".join(f"Bb1a-row-{row:02}  \n" for row in range(60))+REPLY
DEFAULT_EVIDENCE = Path(tempfile.gettempdir()) / "rustain-21-4-smoke"
SCRATCH = Path(tempfile.gettempdir())
# A long TMPDIR is fine for evidence, but not for an AF_UNIX workspace socket.
if len(os.fsencode(SCRATCH / "r214-12345678/a/data/daemons" / ("0"*16+".sock"))) >= 108:
    SCRATCH = Path("/tmp")


def _evidence_parent():
    """Keep the explicit publication override independent of workspace sockets."""
    return Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", DEFAULT_EVIDENCE)).resolve()


def _smoke_paths(part):
    """Select workspace scratch and publication parent without starting a smoke."""
    parent = _evidence_parent()
    if part == "bb1b-ii" and "RUSTAIN_CAPTURE_EVIDENCE_DIR" not in os.environ:
        parent = DEFAULT_EVIDENCE / "smoke-evidence-4bb1b-ii"
    if part == "c":
        if parent.name != "smoke-evidence-4c":
            parent /= "smoke-evidence-4c"
    return SCRATCH, parent


def _box(screen, title, rail, closing=None):
    """Require the exact A8 sheet/centered rectangle, including every border."""
    rows = screen.split("\n")
    if len(rows) not in (16, 24, 30) or any(len(row) != len(rows[0]) for row in rows):
        return None
    width, region_height = len(rows[0]), len(rows)-4-_strip_rows(screen)
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


def _bb1a_shell(screen, draft):
    """Complete shell, including a full eight-line composer, not a title hit."""
    rows = screen.split("\n")
    if (len(rows) not in (16, 24, 30) or len(rows[0]) not in (60, 80, 130)
            or any(len(row) != len(rows[0]) for row in rows)):
        return None
    top = harness.composer_top(rows)
    if (top is None or top < 2 or not rows[top].endswith("┐")
            or not re.search(r"\bReady\b", rows[top-1])
            or rows[-1] != "└"+"─"*(len(rows[0])-2)+"┘"
            or any(not (r.startswith("│") and r.endswith("│")) for r in rows[top+1:-1])
            or [r[1:-1].strip() for r in rows[top+1:-1]] != ([line.strip() for line in draft.split("\n")] if draft else [""])):
        return None
    return rows, top-1-_strip_rows(screen)


def _strip_rows(screen):
    """Story 21.7: 1 when the mission strip row sits above the status row (the Stage region
    every frame and menu is laid out in ends above it), 0 when it yielded to the Stage."""
    return 1 if harness.strip_row(screen) else 0


def _bb1a_search(screen, query, count, hidden, draft=""):
    shell = _bb1a_shell(screen, draft)
    if not shell or not hidden:
        return None
    rows, status = shell
    prefix = "Search: "+query
    expected = prefix+" "*(len(rows[0])-len(prefix)-len(count))+count
    owners = [y for y, row in enumerate(rows[:status]) if row.startswith("Search: ")]
    return tuple(rows[:status]) if owners == [0] and rows[0] == expected else None


def _bb1a_match(screen, hidden, draft=""):
    stage = _bb1a_search(screen, "A2b", "2/2", hidden, draft)
    # Row zero can share the overpainted Stage-top line when no metadata leads it.
    return stage if stage and "Bb1a-row-01" in "\n".join(stage[1:]) and REPLY not in screen else None


def _bb1a_base(screen, draft):
    shell = _bb1a_shell(screen, draft)
    if not shell or any(token in screen for token in ("Search: ", "┌ Reverse Search ")):
        return None
    return tuple(shell[0][:shell[1]])


def _bb1a_reverse(screen, query, selected, caret_cells, hidden, draft=DRAFT, position=None):
    shell = _bb1a_shell(screen, draft)
    if not shell or not hidden:
        return None
    rows, status = shell
    box = _bottom_box(screen, "Reverse Search", len(rows[0]), status)
    # strip() removes the query prefix's trailing space in the unfiltered state.
    if not box or box[1][1:-1].strip() != ("(reverse-search): "+query).rstrip():
        return None
    top = status-len(box)
    position = len(query) if position is None else position
    if {(x, y) for x, y in caret_cells if y == top+1} != {(19+position, top+1)}:
        return None
    selected_rows = [row[1:-1].strip() for row in box[2:-1] if "▸ " in row]
    if selected is None:
        return box if not selected_rows else None
    # Prefix and closing quote survive ellipsis; never require a long entry tail.
    return box if (len(selected_rows) == 1 and selected_rows[0].startswith('▸ "'+selected)
                   and selected_rows[0].endswith('"')) else None


def _reversed_cells(tui):
    return [(x, y) for y in range(tui.screen.lines) for x in range(tui.screen.columns)
            if tui.screen.buffer[y][x].reverse]


def _deskcheck_bb1a_frames(directory):
    """Offline only: runpy/import entry point; consumes complete Rust exports."""
    stages = {"before", "search-unfiltered", "search-empty", "search-filtered",
              "search-navigating-1", "search-navigating-2", "search-restored", "search-kept",
              "reverse-open", "reverse-filtered", "reverse-cycled", "reverse-accepted", "reverse-restored"}
    records = [json.loads(p.read_text()) for p in Path(directory).glob("bb1a-*.json")]
    frames = {(r["width"], r["height"], r["full"], r["stage"]): r for r in records}
    assert len(records) == len(frames) == 6*len(stages), frames.keys()
    for width, height in [(60, 16), (80, 24), (130, 30)]:
        for full in (False, True):
            def record(stage):
                r = frames[width, height, full, stage]
                assert _bb1a_shell(r["screen"], r["draft"]), (stage, r)
                assert len(r["screen"].split("\n")) == height
                assert len(r["screen"].split("\n")[0]) == width
                return r
            def search(stage, query, count):
                r = record(stage)
                assert _bb1a_search(r["screen"], query, count, r["cursor_hidden"], r["draft"]), r
                return r
            before = record("before")
            assert _bb1a_base(before["screen"], before["draft"])
            for stage, query, count in [("search-unfiltered", "", "0/0"), ("search-empty", "zzzz", "No matches found"),
                                        ("search-filtered", "A2b", "1/2"), ("search-navigating-1", "A2b", "1/2"),
                                        ("search-navigating-2", "A2b", "2/2")]:
                r = search(stage, query, count)
                predicate = lambda s: _bb1a_search(s, query, count, r["cursor_hidden"], r["draft"])
                assert not predicate(before["screen"]), "independent no-owner control"
                rows = r["screen"].split("\n")
                for y in (0, harness.composer_top(rows)-1, len(rows)-1):
                    torn = rows.copy(); torn[y] = " "*width
                    assert not predicate("\n".join(torn)), (stage, y)
                assert not _bb1a_search(r["screen"], query, count, False, r["draft"])
                assert not _bb1a_base(r["screen"], r["draft"]), "live owner cannot be a restored base"
            matched = record("search-navigating-2")
            assert _bb1a_match(matched["screen"], matched["cursor_hidden"], matched["draft"])
            assert not _bb1a_match(before["screen"], True, before["draft"])
            assert not _bb1a_match(matched["screen"].replace("Bb1a-row-01", " "*len("Bb1a-row-01")),
                                  True, matched["draft"]), "torn changed-view anchor"
            restored, kept = record("search-restored"), record("search-kept")
            assert _bb1a_base(restored["screen"], restored["draft"]) == _bb1a_base(before["screen"], before["draft"])
            assert _bb1a_base(kept["screen"], kept["draft"]) != _bb1a_base(before["screen"], before["draft"])
            assert _bb1a_base(kept["screen"], kept["draft"])[1:] == _bb1a_match(
                matched["screen"], matched["cursor_hidden"], matched["draft"])[1:]
            for stage, query, selected in [("reverse-open", "", None), ("reverse-filtered", "a1-", "/a1-third-invalid"),
                                            ("reverse-cycled", "a1-", "/a1-second-invalid")]:
                r = record(stage)
                predicate = lambda s: _bb1a_reverse(s, query, selected, r["reversed_cells"], r["cursor_hidden"], r["draft"])
                box = predicate(r["screen"])
                assert box, r
                assert not predicate(before["screen"]), "independent no-popup control"
                rows = r["screen"].split("\n"); bottom = harness.composer_top(rows)-1
                for y in (bottom-len(box), bottom-len(box)+1, bottom-1, bottom, len(rows)-1):
                    torn = rows.copy(); torn[y] = " "*width
                    assert not predicate("\n".join(torn)), (stage, y)
                if selected:
                    assert not predicate(r["screen"].replace('▸ "'+selected, '  "'+selected))
                assert not _bb1a_reverse(r["screen"], query, selected, r["reversed_cells"], False, r["draft"])
                assert not _bb1a_base(r["screen"], r["draft"])
                assert not _bb1a_reverse(r["screen"], query, selected, [(0, 0)], True, r["draft"])
            accepted, rollback = record("reverse-accepted"), record("reverse-restored")
            assert _bb1a_base(accepted["screen"], "/a1-second-invalid") and not accepted["cursor_hidden"]
            assert _bb1a_base(rollback["screen"], rollback["draft"]) and not rollback["cursor_hidden"]
            assert accepted["cursor"] == [len("/a1-second-invalid")+2, height-2]
            assert rollback["cursor"] == [2 if full else len(DRAFT)+1, height-2]
            for r in (before, restored, kept, accepted, rollback):
                rows = r["screen"].split("\n")
                for y in (harness.composer_top(rows)-1, len(rows)-2, len(rows)-1):
                    torn = rows.copy(); torn[y] = " "*width
                    assert not _bb1a_base("\n".join(torn), r["draft"])
    return len(frames)


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
        # 21.7: the mission strip row shortens the Stage by one, so the final row is the previous one.
        return first == "close." and last.startswith("Shift+Wheel   Scroll half page per tick")
    if width == 130:
        return first == "" and last.startswith("G             Jump to newest message")
    return False


def _model_box(screen, title="Select Model"):
    body = _box(screen, title, "Esc close")
    return body if body and any(row[1:-1].strip().startswith("▸ ") for row in body) else None


def _palette_unfiltered(screen):
    body = _box(screen, HELP+" › Command Palette", "Up/Down move  Enter run  Esc close", closing="1/")
    count = re.search(r"(?m)^ *└ 1/([1-9]\d*) ─+┘ *$", screen)
    return body if (body and count and body[0][1:-1].strip() == ">"
                    and 3 < sum(bool(row[1:-1].strip()) for row in body) <= int(count[1])+2) else None


def _palette_models(screen):
    body = _box(screen, HELP+" › Command Palette", "")
    if not body:
        return None
    content = [row[1:-1].strip() for row in body if row[1:-1].strip()]
    return body if (len(content) == 3 and content[0] == "> /models"
                    and content[1].split()[0] == "/models"
                    and all(key in content[-1] for key in ("Enter", "run", "Esc", "close"))) else None


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
    """The budget decision is an ordinary L3 frame over Help (Story 21.6): the exact
    A8 sheet/centered rectangle, never the old bottom-anchored box."""
    box = _box(screen, BUDGET, "[Esc] Later")
    if not box:
        return None
    content = [row[1:-1].strip() for row in box if row[1:-1].strip()]
    text = " ".join(content)
    return box if ("Daily budget ($0.00) reached." in text
                   and all(pair in text for pair in BUDGET_RAIL)) else None


def _popup_box(screen):
    rows = screen.split("\n")
    box = _bottom_box(screen, "Commands", len(rows[0]), len(rows)-4)
    return box if box and len(box) == 3 and box[-2][1:-1].strip().startswith("/detach  ") else None


def _leader_box(screen, feedback=None):
    rows = screen.split("\n")
    box = _bottom_box(screen, "Ctrl+X", len(rows[0]), len(rows)-4-_strip_rows(screen), feedback)
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
    prefix = re.compile(rf"^{re.escape(text)}(?:\s|…|│|$)")
    return any(prefix.match(cell.lstrip()) for row in harness.toast_rows(screen, rows=3)
               for cell in row.split("│"))


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
    _deskcheck_ba_frames()
    _deskcheck_saved_frames()
    directory = os.environ["RUSTAIN_MODAL_STACK_BB2A_FRAMES"]
    _deskcheck_bb1a_frames(directory)
    _deskcheck_bb2a_frames(directory)


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



def _deskcheck_ba_frames():
    frames = {}
    for line in Path(os.environ["RUSTAIN_MODAL_STACK_BA_FRAMES"]).read_text().splitlines():
        if line.startswith("BA_DESKCHECK_FRAME="):
            width, height, stage, screen = json.loads(line.split("=", 1)[1])
            assert len(screen.split("\n")) == height
            assert all(len(row) == width for row in screen.split("\n"))
            assert (width, height, stage) not in frames
            frames[width, height, stage] = screen
    assert len(frames) == 24, frames.keys()
    for width, height in [(60, 16), (80, 24), (130, 30)]:
        def frame(stage):
            screen = frames[width, height, stage]
            assert _status(screen) and _composer(screen) == DRAFT, (stage, screen)
            return screen
        for stage, title in [("model-alone", "Select Model"), ("help-model", HELP+" › Select Model")]:
            screen = frame(stage)
            body = _model_box(screen, title)
            assert body, (stage, screen)
            rows = screen.split("\n")
            top = next(y for y, row in enumerate(rows) if "┌ "+title+" " in row)
            bottom = top+len(body)+1
            selected = next(y for y in range(top+1, bottom) if "▸ " in rows[y])
            footer = next(y for y in range(top+1, bottom) if "Esc close" in rows[y])
            for y in (top, top+1, bottom, selected, footer):
                torn = rows.copy(); torn[y] = " "*width
                assert _model_box("\n".join(torn), title) is None
        palette = frame("palette-models")
        assert _palette_models(palette), palette
        for before, after in [("> /models", "> /model "), ("/models ", "/model  "), ("Enter", "     ")]:
            assert _palette_models(palette.replace(before, after)) is None
        rows = palette.split("\n")
        bottom = next(y for y, row in enumerate(rows[:-4]) if "┘" in row)
        rows[bottom] = " "*width
        assert _palette_models("\n".join(rows)) is None
        assert _help_box(frame("help-before")) == _help_box(frame("help-return"))
        base = frame("base-return")
        assert HELP not in base and "Select Model" not in base and "Command Palette" not in base

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
        self._draw_tail = b""
        self.hidden_draw_complete = False
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
        # Crossterm ends a hidden-cursor draw with HideCursor. Borders alone
        # can already match halfway through a scroll; carry split CSI bytes.
        self._draw_tail = (self._draw_tail+data)[-6:]
        self.hidden_draw_complete = self._draw_tail == b"\x1b[?25l"
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
        (self.evidence / f"{self.tag}-{label}.frame.json").write_text(json.dumps({
            "screen": screen, "cursor_hidden": self.screen.cursor.hidden,
            "cursor": [self.screen.cursor.x, self.screen.cursor.y],
            "reversed_cells": _reversed_cells(self)}, sort_keys=True)+"\n")

    def resize(self, width, height):
        before = [self.screen.columns, self.screen.lines]
        self.screen.resize(lines=height, columns=width)
        self.child.setwinsize(height, width)
        self.frames.write(json.dumps({"at": time.monotonic(), "resize_from": before,
                                      "viewport": [width, height]})+"\n")
        self.frames.flush()

    def exit(self, draft=None):
        self.send("guarded final quit", b"\x11")
        predicate = (lambda s: _bbii_quit(s, self.screen.cursor.hidden, draft)) if draft is not None else (
            lambda s: _box(s, "System · Quit", "Stay") and _status(s))
        self.wait(predicate, "settled final quit question and complete keys")
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
    if not attached:
        _ba_leg(tui)
        _bb1a_leg(tui)
    _bb2a_leg(tui)
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
        screen = tui.wait(lambda s: (box := _box(s, HELP+" › Needs you", "[Up/Down] Select"))
                          and any("▸ Daily budget ·" in row for row in box)
                          and any("[Enter] Open" in row and "[Esc] Back" in row for row in box)
                          and _status(s) and _cue(s) == 1 and tui.screen.cursor.hidden,
                          "frame-origin q opens the selected budget in the Needs-you list")
        tui.settle(lambda s: (box := _box(s, HELP+" › Needs you", "[Up/Down] Select"))
                   and any("▸ Daily budget ·" in row for row in box)
                   and _status(s) and _cue(s) == 1 and tui.screen.cursor.hidden,
                   0.6, "Needs-you list beyond its inert guard")
        tui.send("open selected budget", b"\r")
        screen = tui.wait(lambda s: _budget_box(s) and _status(s) and _cue(s) == 1
                          and tui.screen.cursor.hidden, "Enter opens the deferred budget")
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
                          "truthful No decisions waiting in frame-origin menu; no pane")
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


def _ba_leg(tui):
    tui.send("Ba direct model opener", b"\x18m")
    screen = tui.wait(lambda s: _model_box(s) and _status(s) and _composer(s) == DRAFT
                      and tui.screen.cursor.hidden, "Ba complete Standard model sheet")
    tui.capture("20-ba-model-sheet", screen)
    tui.send("Ba direct model back", b"\x1b")
    tui.wait(lambda s: "Select Model" not in s and _composer(s) == DRAFT
             and not tui.screen.cursor.hidden, "Ba direct opener restores base")
    tui.send("Ba caller Help", b"\x18?")
    tui.wait(lambda s: _help_box(s) and _status(s) and tui.screen.cursor.hidden, "Ba Help caller ready")
    tui.send("Ba caller scroll", b"jjj")
    screen = tui.wait(lambda s: _help_scrolled(s) and _status(s), "Ba exact scrolled Help caller")
    caller = _help_box(screen)
    tui.capture("21-ba-help-caller", screen)
    tui.send("Ba nested palette", b"\x10")
    tui.wait(lambda s: _palette_unfiltered(s) and _status(s)
             and tui.screen.cursor.hidden, "Ba palette above Help")
    tui.send("Ba unique models palette entry", b"/models")
    screen = tui.wait(lambda s: _palette_models(s) and _status(s)
                      and tui.screen.cursor.hidden, "Ba settled unique models entry")
    tui.capture("22-ba-models-entry", screen)
    tui.send("Ba palette action handoff", b"\r")
    screen = tui.wait(lambda s: _model_box(s, HELP+" › Select Model") and _status(s)
                      and "Command Palette" not in s and _composer(s) == DRAFT
                      and tui.screen.cursor.hidden, "Ba handoff closes palette, retains Help caller")
    tui.capture("23-ba-model-handoff", screen)
    tui.send("Ba model returns to palettes caller", b"\x1b")
    screen = tui.wait(lambda s: _help_box(s) == caller and "Select Model" not in s
                      and "Command Palette" not in s and _status(s)
                      and tui.screen.cursor.hidden, "Ba exact Help caller restored, not palette")
    tui.capture("24-ba-exact-caller", screen)
    tui.send("Ba close restored caller", b"\x1b")
    screen = tui.wait(lambda s: HELP not in s and _composer(s) == DRAFT
                      and not tui.screen.cursor.hidden, "Ba caller returns to unchanged draft")
    tui.capture("25-ba-final-draft", screen)


def _bb1a_leg(tui):
    """Local Search Back/Keep and routed history; attached availability unchanged."""
    tui.send("Bb1a clear retained draft for Search", b"\x7f"*len(DRAFT))
    before = tui.wait(lambda s: _bb1a_base(s, "") and REPLY in s
                      and not tui.screen.cursor.hidden, "Bb1a bottom rows and empty Input")
    before_stage = _bb1a_base(before, "")
    tui.capture("26-bb1a-before-search", before)
    for keep in (False, True):
        tui.send("Bb1a real Ctrl+F", b"\x06")
        tui.wait(lambda s: _bb1a_search(s, "", "0/0", tui.screen.cursor.hidden),
                 "Bb1a complete unfiltered Search owner")
        tui.send("Bb1a query matches user and long assistant", b"A2b")
        tui.wait(lambda s: _bb1a_search(s, "A2b", "1/2", tui.screen.cursor.hidden),
                 "Bb1a filtered two-message counter")
        tui.send("Bb1a commit Typing to Navigating", b"\r")
        tui.wait(lambda s: _bb1a_search(s, "A2b", "1/2", tui.screen.cursor.hidden),
                 "Bb1a Navigating first match")
        tui.send("Bb1a navigate to assistant start, not bottom", b"n")
        navigated = tui.wait(lambda s: _bb1a_match(s, tui.screen.cursor.hidden),
                             "Bb1a 2/2 long assistant start and absent bottom marker")
        matched = _bb1a_match(navigated, tui.screen.cursor.hidden)
        assert matched[1:] != before_stage[1:], "navigation must actually change Stage rows"
        tui.capture("29-bb1a-keep-match" if keep else "27-bb1a-back-match", navigated)
        tui.send("Bb1a Navigating Enter keeps" if keep else "Bb1a Esc restores", b"\r" if keep else b"\x1b")
        if keep:
            screen = tui.wait(lambda s: _bb1a_base(s, "") and not tui.screen.cursor.hidden
                              and _bb1a_base(s, "")[1:] == matched[1:]
                              and _bb1a_base(s, "") != before_stage,
                              "Bb1a Enter closes owner and keeps exact observable match rows")
        else:
            screen = tui.wait(lambda s: _bb1a_base(s, "") == before_stage
                              and not tui.screen.cursor.hidden, "Bb1a Esc exact pre-search rows")
        tui.capture("30-bb1a-enter-kept" if keep else "28-bb1a-esc-restored", screen)
    tui.send("Bb1a restore original retained draft", DRAFT.encode())
    tui.wait(lambda s: _bb1a_base(s, DRAFT) and not tui.screen.cursor.hidden, "Bb1a draft restored")
    carets = lambda: _reversed_cells(tui)
    tui.send("Bb1a routed Ctrl+R", b"\x12")
    screen = tui.wait(lambda s: _bb1a_reverse(s, "", None, carets(), tui.screen.cursor.hidden),
                      "Bb1a complete Input popup, unchanged composer, query cursor")
    tui.capture("31-bb1a-reverse-open", screen)
    tui.send("Bb1a history filter", b"a1-")
    screen = tui.wait(lambda s: _bb1a_reverse(s, "a1-", "/a1-third-invalid", carets(), tui.screen.cursor.hidden),
                      "Bb1a filtered newest history selected")
    tui.capture("32-bb1a-reverse-filtered", screen)
    tui.send("Bb1a cycle real history selection", b"\x12")
    screen = tui.wait(lambda s: _bb1a_reverse(s, "a1-", "/a1-second-invalid", carets(), tui.screen.cursor.hidden),
                      "Bb1a older history selection settled")
    tui.capture("33-bb1a-reverse-cycled", screen)
    tui.send("Bb1a accept selected history into composer only", b"\r")
    screen = tui.wait(lambda s: _bb1a_base(s, "/a1-second-invalid") and not tui.screen.cursor.hidden
                      and tui.screen.cursor.x == len("/a1-second-invalid")+2,
                      "Bb1a selected history accepted, popup closed, composer caret")
    tui.capture("34-bb1a-reverse-accepted", screen)
    # The inherited notice commands submitted their autocomplete delimiter too.
    tui.send("Bb1a replace accepted text with retained draft", b"\x7f"*len("/a1-second-invalid ")+DRAFT.encode())
    tui.wait(lambda s: _bb1a_base(s, DRAFT) and not tui.screen.cursor.hidden, "Bb1a draft before popup cancel")
    tui.send("Bb1a reopen history", b"\x12")
    tui.wait(lambda s: _bb1a_reverse(s, "", None, carets(), tui.screen.cursor.hidden), "Bb1a popup reopened")
    tui.send("Bb1a refilter before Esc", b"a1-")
    tui.wait(lambda s: _bb1a_reverse(s, "a1-", "/a1-third-invalid", carets(), tui.screen.cursor.hidden),
             "Bb1a popup query settled before cancel")
    tui.send("Bb1a Esc dismisses popup only", b"\x1b")
    screen = tui.wait(lambda s: _bb1a_base(s, DRAFT) and not tui.screen.cursor.hidden
                      and tui.screen.cursor.x == len(DRAFT)+1, "Bb1a Esc preserves original draft and Input caret")
    tui.capture("35-bb1a-final-draft", screen)


def _bb1b_frame(screen, hidden, draft, title, palette=False):
    """The same Stage geometry at all sizes, including the full composer."""
    shell = _bb1a_shell(screen, draft)
    if not shell or not hidden:
        return None
    rows, region = shell
    width = len(rows[0])
    w, h = ((width, region) if width < 100 else
            (width*9//10, min(region, max(5, region*85//100))))
    left, top = (width-w)//2, (region-h)//2
    box = tuple(row[left:left+w] for row in rows[top:top+h])
    prefix = "┌ "+title+" "
    if (len(box) < 5 or box[0] != prefix+"─"*(w-len(prefix)-1)+"┐"
            or not (box[-1] == "└"+"─"*(w-2)+"┘" or
                    (palette and re.fullmatch(r"└ [1-9]\d*/[1-9]\d* ─+┘", box[-1])))
            or any(not (r.startswith("│") and r.endswith("│")) for r in box[1:-1])):
        return None
    return box


def _bb2a_palette(screen, query, position, mode, caret_cells, hidden, draft=DRAFT):
    title = "Commands /" if query.startswith("/") and mode != "empty" else "Command Palette"
    box = _bb1b_frame(screen, hidden, draft, title, palette=True)
    if not box:
        return None
    content = [row[1:-1].strip() for row in box[1:-1] if row[1:-1].strip()]
    if (len(content) < 3 or content[0] != ("> "+query).rstrip()
            or not all(key in content[-1] for key in ("Up/Down", "Enter", "Esc"))):
        return None
    results = content[1:-1]
    if mode == "empty":
        if results != ["No matches"]:
            return None
    elif (not results or results[0] == "No matches" or len(results[0].split()) < 2 or
          (mode == "filtered" and results[0].split()[0] != "/ml")):
        return None
    rows, region = _bb1a_shell(screen, draft)
    top, left = (region-len(box))//2, (len(rows[0])-len(box[0]))//2
    if {(x, y) for x, y in caret_cells if y == top+1} != {(left+3+position, top+1)}:
        return None
    return box


def _bb2a_search(screen, query, position, count, caret_cells, hidden, draft=""):
    stage = _bb1a_search(screen, query, count, hidden, draft)
    return stage if (stage and {(x, y) for x, y in caret_cells if y == 0}
                     == {(8+position, 0)}) else None


def _deskcheck_bb2a_frames(directory):
    """Real same-manager exports, styled-caret controls and torn complete frames."""
    cases = {
        "search-unfiltered": ("", 0, "0/0"), "search-empty": ("zzzz", 4, "No matches found"),
        "search-filtered": ("A2b", 3, "1/2"), "search-caret": ("A2b", 1, "1/2"),
        "search-pasted": ("Ax2b", 2, "No matches found"), "search-refiltered": ("A2b", 1, "1/2"),
        "reverse-unfiltered": ("", 0, None), "reverse-filtered": ("a1-", 3, "/a1-third-invalid"),
        "reverse-caret": ("a1-", 1, "/a1-third-invalid"), "reverse-pasted": ("ax1-", 2, None),
        "reverse-refiltered": ("a1-", 1, "/a1-third-invalid"),
        "palette-unfiltered": ("", 0, "some"), "palette-filtered": ("/ml", 3, "filtered"),
        "palette-caret": ("/ml", 1, "filtered"), "palette-pasted": ("/~ml", 2, "empty"),
        "palette-killed": ("/~", 2, "empty"), "palette-cleared": ("", 0, "some"),
    }
    records = [json.loads(path.read_text()) for path in Path(directory).glob("bb2a-*.json")]
    frames = {(r["width"], r["height"], r["full"], r["stage"]): r for r in records}
    assert len(records) == len(frames) == 6*len(cases), frames.keys()
    old = {(r["width"], r["height"], r["full"], r["stage"]): r
           for path in Path(directory).glob("bb1a-*.json") for r in [json.loads(path.read_text())]}
    counted = False
    for width, height in [(60, 16), (80, 24), (130, 30)]:
        for full in (False, True):
            for kind in ("palette", "search", "reverse"):
                filtered, moved = (frames[width, height, full, f"{kind}-{name}"]
                                   for name in ("filtered", "caret"))
                assert filtered["screen"] == moved["screen"], "motion changes styles, not field text or Stage"
                assert filtered["reversed_cells"] != moved["reversed_cells"], "observable caret-motion control"
            for label, (query, position, mode) in cases.items():
                r = frames[width, height, full, label]
                kind = label.split("-")[0]
                def predicate(screen, cells=r["reversed_cells"], hidden=r["cursor_hidden"]):
                    if kind == "palette":
                        return _bb2a_palette(screen, query, position, mode, cells, hidden, r["draft"])
                    if kind == "search":
                        return _bb2a_search(screen, query, position, mode, cells, hidden, r["draft"])
                    return _bb1a_reverse(screen, query, mode, cells, hidden, r["draft"], position)
                box = predicate(r["screen"])
                assert box, (label, r)
                assert not predicate(r["screen"], cells=[]), (label, "missing actual caret style")
                assert not predicate(r["screen"], cells=[(0, 0)]), (label, "wrong caret cell")
                assert not predicate(r["screen"], hidden=False), (label, "second hardware cursor")
                base = old[width, height, full, "before" if kind == "search" else "reverse-restored"]
                assert base["draft"] == r["draft"]
                assert not predicate(base["screen"]), (label, "independent no-owner control")
                rows, region = _bb1a_shell(r["screen"], r["draft"])
                if kind == "search":
                    edges = [0, region, height-1]
                else:
                    top = (region-len(box))//2 if kind == "palette" else region-len(box)
                    edges = [top, top+1, top+len(box)-1, region, height-1]
                    if kind == "palette":
                        edges += [next(y for y in range(top+1, top+len(box)-1)
                                       if all(key in rows[y] for key in ("Up/Down", "Enter", "Esc")))]
                        counted |= bool(re.fullmatch(r"└ [1-9]\d*/[1-9]\d* ─+┘", box[-1]))
                for y in set(edges):
                    torn = rows.copy(); torn[y] = " "*width
                    assert not predicate("\n".join(torn)), (label, width, full, y)
    assert counted, "the real unfiltered palette exercises its counted lower rail"
    return len(frames)


def _bb2a_leg(tui):
    """Both real palette hosts; local Search calm jumps/Back and history editing."""
    def palette(query, position, mode):
        return lambda s: _bb2a_palette(s, query, position, mode, _reversed_cells(tui), tui.screen.cursor.hidden)
    tui.send("Bb2a palette", b"\x10")
    screen = tui.wait(palette("", 0, "some"), "Bb2a complete unfiltered palette with in-widget caret")
    tui.capture("36-bb2a-palette-unfiltered", screen)
    for label, data, query, position, mode in [
        ("filtered", b"\x1b[200~/ml\x1b[201~", "/ml", 3, "filtered"),
        ("caret", b"\x01\x1b[C", "/ml", 1, "filtered"),
        ("pasted", b"\x1b[200~~\x1b[201~", "/~ml", 2, "empty"),
        ("killed", b"\x0b", "/~", 2, "empty"),
        ("cleared", b"\x15", "", 0, "some"),
    ]:
        tui.send("Bb2a palette "+label, data)
        screen = tui.wait(palette(query, position, mode), "Bb2a settled palette "+label+" and actual rows")
        tui.capture("37-bb2a-palette-"+label, screen)
    tui.send("Bb2a palette close", b"\x1b")
    tui.wait(lambda s: _bb1a_base(s, DRAFT) and not tui.screen.cursor.hidden, "Bb2a exact palette draft")
    if tui.tag == "attached":
        return
    tui.send("Bb2a empty composer for Search's legacy opener", b"\x15")
    before = tui.wait(lambda s: _bb1a_base(s, "") and not tui.screen.cursor.hidden, "Bb2a pre-search Stage")
    before_stage = _bb1a_base(before, "")
    tui.capture("38-bb2a-before-search", before)
    def search(query, position, count):
        return lambda s: _bb2a_search(s, query, position, count, _reversed_cells(tui), tui.screen.cursor.hidden)
    tui.send("Bb2a real Search", b"\x06")
    tui.wait(search("", 0, "0/0"), "Bb2a Search Typing's actual caret")
    tui.send("Bb2a pasted Search content", b"\x1b[200~A2b\x1b[201~")
    filtered = tui.wait(search("A2b", 3, "1/2"), "Bb2a pasted content keeps the calm jump")
    filtered_stage = _bb1a_search(filtered, "A2b", "1/2", True)
    assert filtered_stage[1:] != before_stage[1:], "content really changes the rendered owning Stage"
    tui.capture("39-bb2a-search-content", filtered)
    for label, data, query, position, count in [
        ("caret", b"\x01\x1b[C", "A2b", 1, "1/2"),
        ("pasted", b"\x1b[200~x\x1b[201~", "Ax2b", 2, "No matches found"),
        ("refiltered", b"\x08", "A2b", 1, "1/2"),
    ]:
        tui.send("Bb2a Search "+label, data)
        screen = tui.wait(search(query, position, count), "Bb2a Search "+label+" caret and counter")
        assert _bb1a_search(screen, query, count, True)[1:] == filtered_stage[1:]
        tui.capture("40-bb2a-search-"+label, screen)
    tui.send("Bb2a commit and navigate", b"\x05\rn")
    navigated = tui.wait(lambda s: _bb1a_match(s, tui.screen.cursor.hidden), "Bb2a actual second match")
    tui.send("Bb2a Navigating caret-only returns to Typing without navigation", b"\x01")
    screen = tui.wait(search("A2b", 0, "2/2"), "Bb2a caret-only edit does not rescan")
    assert _bb1a_search(screen, "A2b", "2/2", True)[1:] == _bb1a_match(navigated, True)[1:]
    tui.capture("41-bb2a-search-navigating-caret", screen)
    tui.send("Bb2a exact Search Back", b"\x1b")
    screen = tui.wait(lambda s: _bb1a_base(s, "") == before_stage and not tui.screen.cursor.hidden,
                      "Bb2a Esc restores exact pre-search rows")
    tui.capture("42-bb2a-search-restored", screen)
    tui.send("Bb2a restore interrupted draft", DRAFT.encode())
    tui.wait(lambda s: _bb1a_base(s, DRAFT) and not tui.screen.cursor.hidden, "Bb2a unchanged draft")
    def reverse(query, position, selected):
        return lambda s: _bb1a_reverse(s, query, selected, _reversed_cells(tui),
                                      tui.screen.cursor.hidden, position=position)
    tui.send("Bb2a real ReverseSearch", b"\x12")
    tui.wait(reverse("", 0, None), "Bb2a ReverseSearch owns the sole visible caret")
    for label, data, query, position, selected in [
        ("filtered", b"\x1b[200~a1-\x1b[201~", "a1-", 3, "/a1-third-invalid"),
        ("caret", b"\x01\x1b[C", "a1-", 1, "/a1-third-invalid"),
        ("pasted", b"\x1b[200~x\x1b[201~", "ax1-", 2, None),
        ("refiltered", b"\x08", "a1-", 1, "/a1-third-invalid"),
    ]:
        tui.send("Bb2a ReverseSearch "+label, data)
        screen = tui.wait(reverse(query, position, selected), "Bb2a settled ReverseSearch "+label)
        tui.capture("43-bb2a-reverse-"+label, screen)
    tui.send("Bb2a ReverseSearch Back", b"\x1b")
    screen = tui.wait(lambda s: _bb1a_base(s, DRAFT) and not tui.screen.cursor.hidden,
                      "Bb2a history field closes without modifying the composer")
    tui.capture("44-bb2a-final-draft", screen)


def _bb1b_book(screen, hidden, draft, count, index, payload):
    box = _bb1b_frame(screen, hidden, draft, f"Bookmarks ({count})")
    if not box:
        return None
    selected = [r[1:-1].strip() for r in box[1:-1] if "▸ " in r]
    footer = next((r for r in reversed(box[1:-1]) if r[1:-1].strip()), "")
    # Source tails may ellipsize; the selected durable index and lede may not.
    return box if (len(selected) == 1 and selected[0].startswith(f"▸ msg {index}: {payload}")
                   and all(t in footer for t in ("Jump", "Delete", "Undo", "Close"))) else None


def _bb1b_wave(screen, hidden, draft, kind, slot=0, total=2, payload=None, reversed_rows=()):
    box = _bb1b_frame(screen, hidden, draft, "Wave")
    if not box:
        return None
    body = "\n".join(box[1:-1])
    footer = next((r for r in reversed(box[1:-1]) if r[1:-1].strip()), "")
    list_actions = ("Enter drill", "r rerun", "d diverge", "Esc/Ctrl+C close")
    if kind == "list":
        selected = [screen.split("\n")[y] for y in reversed_rows]
        return box if (box[1][1:-1].strip() == f"▾ {slot+1 if total else 0} of {total}"
                       and (not total or (payload and sum(payload in r and "↵" in r for r in selected) == 1))
                       and all(t in footer for t in list_actions)) else None
    if kind == "drill":
        return box if (box[1][1:-1].strip() == f"Drill: spoke {slot}" and payload in body
                       and (all(t in footer for t in list_actions)
                            or all(t in footer for t in ("Enter/r overlay", "d diverge", "Esc dismiss", "Ctrl+C close")))) else None
    if kind == "diverge":
        return box if (box[1][1:-1].strip() == f"{total} spokes disagree"
                       and payload in body and all(t in footer for t in ("s back", "Esc back", "Ctrl+C back"))) else None
    raise AssertionError(kind)


def _bb1b_base(screen, hidden, draft):
    stage = _bb1a_base(screen, draft)
    # This fixture's plain Stage rows have no sheet corners or right-hand rail.
    # Reject partial sheet erasure, not merely an absent title.
    if not hidden or not stage or any(
            row.lstrip().startswith(("┌", "└")) or row.endswith("│") for row in stage):
        return None
    return stage


def _bb1b_settled_base(screen, hidden, draft):
    stage = _bb1b_base(screen, hidden, draft)
    # Toast expiry is a legitimate paint change, not a change to Tab.view_state.
    return stage if stage and not any(
        text in screen for text in ("Bookmark added", "Mouse scroll enabled.")) else None


def _deskcheck_bb1b_i_frames(directory):
    stages = {"before", "bookmark-open", "bookmark-selected", "bookmark-back", "bookmark-jump",
              "bookmark-counted", "bookmark-empty", "wave-open", "wave-selected", "wave-drill",
              "wave-diverge", "wave-drill-return", "wave-closed", "wave-direct-diverge",
              "wave-compact", "wave-reopen", "wave-list-diverge", "wave-list-compact", "wave-empty", "wave-counted"}
    records = [json.loads(p.read_text()) for p in Path(directory).glob("bb1b-i-*.json")]
    frames = {(r["width"], r["height"], r["full"], r["stage"]): r for r in records}
    assert len(records) == len(frames) == 6*len(stages), frames.keys()
    for width, height in ((60, 16), (80, 24), (130, 30)):
        for full in (False, True):
            def record(stage):
                r = frames[width, height, full, stage]
                assert _bb1a_shell(r["screen"], r["draft"]), r
                assert len(r["screen"].split("\n")) == height and len(r["screen"].split("\n")[0]) == width
                return r
            before = record("before")
            checks = [
                ("bookmark-open", lambda s, r: _bb1b_book(s, r["cursor_hidden"], r["draft"], 2, 0, "stage-0-row-00")),
                ("bookmark-selected", lambda s, r: _bb1b_book(s, r["cursor_hidden"], r["draft"], 2, 2, "stage-2-row-00")),
                ("bookmark-counted", lambda s, r: _bb1b_book(s, r["cursor_hidden"], r["draft"], 3, 2, "stage-2-row-00")),
            ]
            for stage, kind, slot, total in [
                ("wave-open", "list", 0, 2), ("wave-selected", "list", 1, 2),
                ("wave-reopen", "list", 0, 2), ("wave-empty", "list", 0, 0), ("wave-counted", "list", 23, 24),
                ("wave-drill", "drill", 1, 2), ("wave-drill-return", "drill", 1, 2),
                ("wave-diverge", "diverge", 1, 2), ("wave-direct-diverge", "diverge", 0, 2),
                ("wave-list-diverge", "diverge", 0, 2),
            ]:
                payload = NOTICES[slot % 2]
                checks.append((stage, lambda s, r, k=kind, i=slot, n=total, p=payload:
                               _bb1b_wave(s, r["cursor_hidden"], r["draft"], k, i, n, p, r["reversed"])))
            for stage, predicate in checks:
                r = record(stage); box = predicate(r["screen"], r)
                assert box, (stage, r)
                assert not predicate(before["screen"], r), "independent absent-owner control"
                rows = r["screen"].split("\n")
                top = next(y for y, row in enumerate(rows) if box[0] in row)
                status = harness.composer_top(rows)-1
                for y in (top, top+1, top+len(box)-2, top+len(box)-1, status, height-1):
                    torn = rows.copy(); torn[y] = " "*width
                    assert not predicate("\n".join(torn), r), (stage, y)
                footer = next(r[1:-1] for r in reversed(box[1:-1]) if r[1:-1].strip())
                assert not predicate(r["screen"].replace(footer, " "*len(footer)), r), "missing action rail"
                for action in ("Jump", "Delete", "Undo", "Close", "Enter drill", "r rerun",
                               "d diverge", "Esc/Ctrl+C close", "Enter/r overlay", "Esc dismiss",
                               "Ctrl+C close", "s back", "Esc back", "Ctrl+C back"):
                    if action in footer:
                        assert not predicate(r["screen"].replace(action, " "*len(action)), r), (stage, action)
                assert not predicate(r["screen"], {**r, "cursor_hidden": False}), stage
                if stage.startswith("bookmark"):
                    assert not predicate(r["screen"].replace("▸ msg ", "  msg "), r)
                    assert not predicate(r["screen"].replace("▸ msg ", "▸ bad "), r), "wrong durable selection"
                elif stage not in ("wave-empty",):
                    assert not predicate(r["screen"].replace(NOTICES[0], "x"*len(NOTICES[0])).replace(
                        NOTICES[1], "x"*len(NOTICES[1])), r), "incorrect selected/payload control"
            for stage in ("before", "bookmark-back", "bookmark-jump", "bookmark-empty",
                          "wave-closed", "wave-compact", "wave-list-compact"):
                r = record(stage)
                assert _bb1b_base(r["screen"], r["cursor_hidden"], r["draft"]), r
                assert _bb1b_settled_base(r["screen"], r["cursor_hidden"], r["draft"]), r
                rows = r["screen"].split("\n")
                for border in ("┌"+"─"*(width-2)+"┐", "└"+"─"*(width-2)+"┘"):
                    torn = rows.copy(); torn[0] = border
                    assert not _bb1b_base("\n".join(torn), True, r["draft"]), "leftover sheet border"
                torn = rows.copy(); torn[0] = torn[0][:-1]+"│"
                assert not _bb1b_base("\n".join(torn), True, r["draft"]), "leftover sheet rail"
                for notice in ("Success Bookmark added (msg 1)", "Info Mouse scroll enabled."):
                    transient = rows.copy(); transient[harness.composer_top(rows)-2] = notice.ljust(width)
                    screen = "\n".join(transient)
                    assert _bb1b_base(screen, True, r["draft"]), "complete but transient control"
                    assert not _bb1b_settled_base(screen, True, r["draft"]), "toast cannot define the restore baseline"
            assert _bb1a_base(record("bookmark-back")["screen"], before["draft"]) == _bb1a_base(before["screen"], before["draft"])
            assert _bb1a_base(record("bookmark-jump")["screen"], before["draft"]) != _bb1a_base(before["screen"], before["draft"])
            selected = record("wave-selected")
            assert not _bb1b_wave(selected["screen"], True, selected["draft"], "list", 0, 2,
                                  NOTICES[0], selected["reversed"]), "wrong actual selection"
    return len(frames)


def _bb1b_i_leg(tui):
    """Local-only front doors; attached Bookmark/Wave are intentionally n/a."""
    def reverse_rows():
        return [y for y, row in enumerate(tui.screen.display)
                if any(tui.screen.buffer[y][x].reverse for x in range(len(row)))]
    def book(count, index, payload):
        return lambda s: _bb1b_book(s, tui.screen.cursor.hidden, DRAFT, count, index, payload)
    def wave(kind, slot=0, payload=NOTICES[0]):
        return lambda s: _bb1b_wave(s, tui.screen.cursor.hidden, DRAFT, kind, slot, 2, payload, reverse_rows())
    def base(s):
        return _bb1b_base(s, tui.screen.cursor.hidden, DRAFT)
    def retained(s):
        return all(any(p in row and "↵" in row for row in s.split("\n")) for p in NOTICES[:2])
    def step(label, data, predicate):
        tui.send(label, data)
        screen = tui.wait(predicate, label)
        tui.capture("bb1b-i-"+label, screen)
        return screen
    # The pre-existing real model turn supplies user msg 0 / assistant msg 1.
    step("chat-focus", b"\x1b", base)
    step("chat-oldest", b"g", lambda s: base(s) and TURN in s and REPLY not in s)
    step("bookmark-user", b"m", lambda s: base(s) and "Bookmark added (msg 0)" in s)
    step("chat-bottom", b"G", lambda s: base(s) and REPLY in s)
    step("bookmark-assistant", b"m", lambda s: base(s) and "Bookmark added (msg 1)" in s)
    screen = tui.wait(
        lambda s: _bb1b_settled_base(s, tui.screen.cursor.hidden, DRAFT) and REPLY in s,
        "bookmark baseline after real transient notices expire")
    tui.capture("bb1b-i-bookmark-settled-baseline", screen)
    before = _bb1a_base(screen, DRAFT)
    step("bookmark-open", b"'", book(2, 0, TURN))
    step("bookmark-selected", b"j", book(2, 1, "Bb1a-row-00"))
    step("bookmark-back", b"\x1b", lambda s: base(s) and _bb1a_base(s, DRAFT) == before)
    step("bookmark-reopen", b"'", book(2, 0, TURN))
    step("bookmark-jump", b"\r", lambda s: base(s) and TURN in s and REPLY not in s
         and _bb1a_base(s, DRAFT) != before)
    step("input-for-fanout", b"i"+b"\x7f"*len(DRAFT), lambda s: _composer(s) == "" and not tui.screen.cursor.hidden)
    step("fanout-submit", ("/fanout 2 "+TURN).encode(), lambda s: _composer(s) == "/fanout 2 "+TURN)
    step("fanout-completed", b"\r", lambda s: _status(s) and retained(s) and _composer(s) == ""
         and not tui.screen.cursor.hidden)
    step("wave-chat", DRAFT.encode()+b"\x1b", base)
    screen = step("wave-open", b"r", lambda s: all(p in s for p in NOTICES[:2])
                  and any(wave("list", 0, p)(s) for p in NOTICES[:2]))
    # Learn only request scheduling, not selection: subsequent j must select the OTHER actual row.
    first = next(p for p in NOTICES[:2] if any(p in tui.screen.display[y] for y in reverse_rows()))
    second = next(p for p in NOTICES[:2] if p != first)
    assert wave("list", 0, first)(screen)
    step("wave-selected", b"j", wave("list", 1, second))
    step("wave-drill", b"\r", wave("drill", 1, second))
    step("wave-drill-list-keys-close", b"\x1b", base)
    step("wave-enter-open", b"\r", wave("list", 0, first))
    step("wave-list-diverge", b"d", wave("diverge", 0, first))
    step("wave-list-diverge-back", b"\x1b", base)
    step("wave-direct-diverge", b"d", wave("diverge", 0, first))
    step("wave-direct-diverge-back", b"s", base)
    step("wave-reopen", b"r", wave("list", 0, first))
    step("wave-selected-again", b"j", wave("list", 1, second))
    step("wave-drill-again", b"\r", wave("drill", 1, second))
    step("wave-drill-diverge", b"d", wave("diverge", 1, first))
    step("wave-prior-drill-chat-keys", b"\x1b", lambda s: wave("drill", 1, second)(s) and "Esc dismiss" in s)
    step("wave-ordinary-close-retains", b"\x03", base)
    step("wave-retained-reopen", b"r", wave("list", 0, first))
    step("wave-retained-drill", b"\r", wave("drill", 0, first))
    step("wave-retained-diverge", b"d", wave("diverge", 0, first))
    step("wave-return-drill-for-dismiss", b"s", wave("drill", 0, first))
    step("wave-full-dismiss", b"\x1b", lambda s: base(s) and not any(p in s for p in NOTICES[:2]))
    step("wave-no-retained-reopen", b"r", lambda s: base(s) and not any(p in s for p in NOTICES[:2]))
    step("input-restored", b"i", lambda s: _composer(s) == DRAFT and not tui.screen.cursor.hidden)

def _bbii_shell(screen, draft):
    """Actual status/composer boundary, including Monitor's full-height sidebar."""
    rows = screen.split("\n")
    if (len(rows) not in (16, 24, 30) or len(rows[0]) not in (60, 80, 130)
            or any(len(row) != len(rows[0]) for row in rows)):
        return None
    top = harness.composer_top(rows)
    if top is None or top < 2:
        return None
    width = len(rows[0])
    composer_width = rows[top].find("┐")+1
    allowed = (width, width-min(50, max(30, width*3//10))) if width >= 120 else (width,)
    if (composer_width not in allowed
            or not re.fullmatch(r"┌ Message(?: \[ML\])? ─*┐", rows[top][:composer_width])
            or not re.search(r"\bReady\b", rows[top-1][:composer_width])
            or rows[-1][:composer_width] != "└"+"─"*(composer_width-2)+"┘"):
        return None
    body = [r[:composer_width] for r in rows[top+1:-1]]
    if (any(not (r.startswith("│") and r.endswith("│")) for r in body)
            or [r[1:-1].strip() for r in body] != draft.split("\n")):
        return None
    return rows, top-1-_strip_rows(screen)


def _bbii_frame(screen, hidden, draft, title, closing=None, spaced=True):
    shell = _bbii_shell(screen, draft)
    if not shell or not hidden:
        return None
    rows, region = shell
    width = len(rows[0])
    w, h = ((width, region) if width < 100 else
            (width*9//10, min(region, max(5, region*85//100))))
    left, top = (width-w)//2, (region-h)//2
    box = tuple(row[left:left+w] for row in rows[top:top+h])
    prefix = ("┌ "+title+" ") if spaced else ("┌"+title)
    bottom = "└"+(closing or "")
    if (len(box) < 5 or box[0] != prefix+"─"*(w-len(prefix)-1)+"┐"
            or box[-1] != bottom+"─"*(w-len(bottom)-1)+"┘"
            or any(not (r.startswith("│") and r.endswith("│")) for r in box[1:-1])):
        return None
    return box


def _bbii_cross(screen, hidden, draft, query, count=None, selected=None):
    box = _bbii_frame(screen, hidden, draft, "History › Cross-Search", " [Enter] open  [Esc] close ")
    if not box or box[1][1:-1].rstrip() != ("/ "+query).rstrip():
        return None
    summary = count or ("Type at least 2 characters" if len(query) < 2 else "No matches found")
    if box[2][1:-1].strip() != summary:
        return None
    titles = [r[1:-1].strip() for r in box[3:-1] if r[1:-1].strip().startswith("▸ ")]
    if selected is None:
        if titles:
            return None
    elif len(titles) != 1 or not titles[0].startswith("▸ "+selected):
        return None
    # Compare the exact queried owner across Quit, except relative-time ticks.
    return tuple("<relative-time>" if re.fullmatch(r"│    (?:just now|\d+[mhd] ago) *│", r) else r
                 for r in box)


def _bbii_quit(screen, hidden, draft, title="System · Quit"):
    box = _bbii_frame(screen, hidden, draft, title)
    if not box:
        return None
    body = [r[1:-1].strip() for r in box[1:-1] if r[1:-1].strip()]
    return box if (body and body[0] == "Quit rustain?"
                   and body[-1] == "[y] Quit   [n/Esc/Enter] Stay"
                   and any("Your unsent draft will be lost." in r for r in body)) else None


def _bbii_history(screen, hidden, draft, require_entry=True, title=TURN):
    shell = _bbii_shell(screen, draft)
    if not shell or not hidden or (require_entry and not title):
        return None
    rows, status = shell
    if len(rows[0]) != 130 or len(rows) != 30 or "Cross-Search" in screen:
        return None
    # History is a centred L2 frame (Story 21.7 A21), not the sidebar column: its title sits in
    # the top rail without padding; an entry row reads `<title> · <age> · 2 msgs`.
    box = _bbii_frame(screen, hidden, draft, "History", spaced=False)
    if not box:
        return None
    body = [r[1:-1] for r in box[1:-1]]
    if ((require_entry and not any(title in r and "2 msgs" in r for r in body))
            or (not require_entry and not any("No conversations" in r for r in body))
            or any("Cross-Search" in r or "System · Quit" in r for r in rows[:status])):
        return None
    return box[0], box[-1], title if require_entry else "No conversations"


def _bbii_base(screen, hidden, draft):
    shell = _bbii_shell(screen, draft)
    if not shell or not hidden:
        return None
    rows, status = shell
    stage = tuple(rows[:status])
    if any(row.lstrip().startswith(("┌", "└")) or row.endswith("│") for row in stage):
        return None
    return stage


def _deskcheck_bb1b_ii_frames(directory):
    """Only real runtime exports are positive controls; tear every primary edge."""
    required = {"cross-unfiltered", "cross-filtered", "cross-empty", "cross-counted"}
    records = [json.loads(p.read_text()) for p in Path(directory).glob("bb1b-ii-*.json")]
    frames = {(r["width"], r["height"], r["full"], r["stage"]): r for r in records}
    assert len(records) == len(frames), "duplicate frame keys"
    for width, height in ((60, 16), (80, 24), (130, 30)):
        for full in (False, True):
            for stage in required:
                r = frames[width, height, full, stage]
                draft = FULL_DRAFT if full else DRAFT
                assert r["draft"] == draft
                assert len(r["screen"].split("\n")) == height
                assert all(len(row) == width for row in r["screen"].split("\n"))
                query, count, title = {
                    "cross-unfiltered": ("", None, None),
                    "cross-empty": ("zzzz", None, None),
                    "cross-filtered": ("Bb1a", "1/1 matches", TURN),
                    "cross-counted": ("Bb1a", "20/20 matches", TURN),
                }[stage]
                def predicate(screen, hidden=r["cursor_hidden"], q=query, c=count, t=title):
                    return _bbii_cross(screen, hidden, draft, q, c, t)
                assert predicate(r["screen"]), r
                box = _bbii_frame(r["screen"], r["cursor_hidden"], draft, "History › Cross-Search",
                                    " [Enter] open  [Esc] close ")
                rows = r["screen"].split("\n")
                top = next(y for y, row in enumerate(rows) if box[0] in row)
                status = harness.composer_top(rows)-1
                for y in (top, top+1, top+2, top+len(box)-1, status, len(rows)-1):
                    torn = rows.copy(); torn[y] = " "*width
                    assert not predicate("\n".join(torn)), (stage, width, full, y)
                # Border cells, not only entire blank lines, must be checked.
                left = rows[top].index(box[0])
                for y in range(top+1, top+len(box)-1):
                    for x in (left, left+len(box[0])-1):
                        torn = rows.copy()
                        torn[y] = torn[y][:x]+" "+torn[y][x+1:]
                        assert not predicate("\n".join(torn)), (stage, y, x, "side border")
                for token in ("[Enter] open", "[Esc] close"):
                    assert not predicate(r["screen"].replace(token, " "*len(token)))
                assert not predicate(r["screen"], hidden=False), "visible query caret"
                assert not predicate(r["screen"], q=query+"x"), "wrong query"
                assert not predicate(r["screen"], c="19/20 matches"), "wrong counter"
                if title:
                    for before, after in (("▸ "+TURN, "  "+TURN), (TURN, "x"*len(TURN))):
                        assert not predicate(r["screen"].replace(before, after)), "wrong selected title"
                    assert not predicate(r["screen"], t="unrelated-title"), "wrong selected result"
                    unfiltered = frames[width, height, full, "cross-unfiltered"]
                    assert not predicate(unfiltered["screen"]), "independent absent-results owner"
            draft = FULL_DRAFT if full else DRAFT
            filtered = frames[width, height, full, "cross-filtered"]
            for stage, title in (("cross-quit", "History › Cross-Search › System · Quit"),
                                 ("base-quit", "History › System · Quit" if width == 130 else "System · Quit")):
                r = frames[width, height, full, stage]
                assert r["draft"] == draft
                def quit_predicate(screen, hidden=r["cursor_hidden"]):
                    return _bbii_quit(screen, hidden, draft, title)
                assert quit_predicate(r["screen"]), r
                box = _bbii_frame(r["screen"], True, draft, title)
                rows = r["screen"].split("\n")
                top = next(y for y, row in enumerate(rows) if box[0] in row)
                for y in (top, top+len(box)-1, harness.composer_top(rows)-1, len(rows)-1):
                    torn = rows.copy(); torn[y] = " "*width
                    assert not quit_predicate("\n".join(torn)), (stage, width, full, y)
                for token in ("Quit rustain?", "Your unsent draft will be lost.",
                              "[y] Quit", "[n/Esc/Enter] Stay"):
                    assert not quit_predicate(r["screen"].replace(token, " "*len(token))), token
                assert not quit_predicate(r["screen"], hidden=False)
                assert not _bbii_quit(r["screen"], True, draft, "wrong-owner")
                assert not quit_predicate(filtered["screen"]), "independent absent Quit owner"
            untitled = frames[width, height, full, "cross-untitled"]
            assert untitled["conversation_title"] == ""
            assert _bbii_cross(untitled["screen"], True, draft, "Bb1a", "1/1 matches", "(untitled)")
            assert not _bbii_cross(untitled["screen"], True, draft, "Bb1a", "1/1 matches", TURN)
            assert not _bbii_cross(filtered["screen"], True, draft, "Bb1a", "1/1 matches", "(untitled)")
            assert not _bbii_cross(untitled["screen"].replace("▸ (untitled)", "  (untitled)"),
                                   True, draft, "Bb1a", "1/1 matches", "(untitled)")
            if width < 100:
                r = frames[width, height, full, "accepted-base"]
                assert r["draft"] == draft
                def accepted_predicate(screen, hidden=r["cursor_hidden"]):
                    return (_bbii_base(screen, hidden, draft)
                            and "Bb1a-row-0" in screen and REPLY not in screen)
                assert accepted_predicate(r["screen"]), r
                assert not accepted_predicate(r["screen"], hidden=False)
                assert not accepted_predicate(filtered["screen"]), "independent still-owned frame"
                assert not accepted_predicate(r["screen"].replace("Bb1a-row-0", "x"*10))
                rows = r["screen"].split("\n")
                for y in (harness.composer_top(rows)-1, len(rows)-1):
                    torn = rows.copy(); torn[y] = " "*width
                    assert not accepted_predicate("\n".join(torn)), (width, full, y)
                torn = rows.copy(); torn[0] = REPLY.ljust(width)
                assert not accepted_predicate("\n".join(torn)), "completion tail is not the accepted match"
            if width == 130:
                r = frames[width, height, full, "history-return"]
                assert r["draft"] == (FULL_DRAFT if full else DRAFT)
                assert _bbii_history(r["screen"], r["cursor_hidden"], r["draft"], require_entry=False), r
                rows = r["screen"].split("\n")
                box = _bbii_frame(r["screen"], True, r["draft"], "History", spaced=False)
                top = next(y for y, row in enumerate(rows) if box[0] in row)
                for y in (top, top+len(box)-1, harness.composer_top(rows)-1, len(rows)-1):
                    torn = rows.copy(); torn[y] = " "*width
                    assert not _bbii_history("\n".join(torn), True, r["draft"], require_entry=False), y
                assert not _bbii_history(r["screen"], False, r["draft"], require_entry=False)
                assert not _bbii_history(r["screen"].replace("No conversations", "x"*16),
                                         True, r["draft"], require_entry=False)
                populated = frames[width, height, full, "history-populated"]
                def history_predicate(screen, hidden=populated["cursor_hidden"]):
                    return _bbii_history(screen, hidden, draft, title=populated["history_title"])
                assert populated["draft"] == draft
                assert history_predicate(populated["screen"]), populated
                assert not history_predicate(r["screen"]), "independent empty History"
                assert not history_predicate(filtered["screen"]), "independent Cross owner"
                assert not history_predicate(populated["screen"], hidden=False)
                assert not _bbii_history(populated["screen"], True, draft, title="")
                for before, after in ((populated["history_title"], "x"*len(populated["history_title"])), ("2 msgs", "3 msgs")):
                    assert not history_predicate(populated["screen"].replace(before, after))
                rows = populated["screen"].split("\n")
                box = _bbii_frame(populated["screen"], True, draft, "History", spaced=False)
                top = next(y for y, row in enumerate(rows) if box[0] in row)
                for y in (top, top+len(box)-1, harness.composer_top(rows)-1, len(rows)-1):
                    torn = rows.copy(); torn[y] = " "*width
                    assert not history_predicate("\n".join(torn)), (full, y)
    # Read an actual FileSystemStorage serialization, not a hand-written echo of
    # the reader's schema. Negative copies never alter the saved positive.
    persisted_ws = Path(directory) / "persisted-control"
    positive = _bbii_persisted(persisted_ws)
    assert positive and positive["messages"] == 2, persisted_ws
    source = persisted_ws / ".claude" / "sessions" / positive["file"]
    original = json.loads(source.read_text())
    populated = frames[130, 30, True, "history-populated"]
    assert _bbii_history(populated["screen"], True, FULL_DRAFT, title=positive["history_title"])
    untitled = frames[60, 16, True, "cross-untitled"]
    assert _bbii_cross(untitled["screen"], True, FULL_DRAFT, "Bb1a", "1/1 matches", positive["result_title"])
    with tempfile.TemporaryDirectory(prefix="offline-persisted-", dir=Path(directory).parent) as tmp:
        workspace = Path(tmp)
        target = workspace / ".claude" / "sessions" / positive["file"]
        target.parent.mkdir(parents=True)
        for field in ("title", "count", "user", "assistant"):
            record = json.loads(json.dumps(original))
            if field == "title":
                record["title"] = "unrelated-title"
            elif field == "count":
                record["messages"] = record["messages"][:1]
            else:
                record["messages"][0 if field == "user" else 1]["content"] = "unrelated-content"
            target.write_text(json.dumps(record))
            found = _bbii_persisted(workspace)
            if field == "title":
                assert found
                assert not _bbii_history(populated["screen"], True, FULL_DRAFT, title=found["history_title"])
                assert not _bbii_cross(untitled["screen"], True, FULL_DRAFT, "Bb1a", "1/1 matches", found["result_title"])
            else:
                assert not found, field
    return len(frames)


def _bbii_persisted(ws):
    # Runtime turn completion touches SessionIndex and asynchronously saves the
    # flat FileSystemStorage payload before CrossSearch loads it via StoragePort.
    for path in (ws / ".claude" / "sessions").glob("*.meta.json"):
        record = json.loads(path.read_text())
        messages = record.get("messages", [])
        if (len(messages) == 2
                and messages[0].get("content") == TURN and messages[1].get("content") == SEARCH_REPLY):
            return {"file": path.name, "id": record["id"], "title": record["title"], "messages": len(messages),
                    "history_title": record["title"] or "New Conversation",
                    "result_title": record["title"] or "(untitled)"}
    return None


def _bb1b_ii_leg(tui, ws):
    """One real turn; actual supported History door, then full-composer sheets."""
    def cross(query="", count=None, title=None):
        return lambda s: _bbii_cross(s, tui.screen.cursor.hidden, FULL_DRAFT, query, count, title)
    def step(label, data, predicate):
        tui.send("Bb1b-ii "+label, data)
        screen = tui.wait(predicate, label)
        tui.capture("bb1b-ii-"+label, screen)
        return screen
    step("prepare-input-draft", DRAFT.encode(), lambda s: _composer(s) == DRAFT)
    step("full-eight-line-draft", b"\x15\x1b[200~"+FULL_DRAFT.encode()+b"\x1b[201~",
         lambda s: _bbii_shell(s, FULL_DRAFT) and REPLY in s and not tui.screen.cursor.hidden)
    tui.resize(130, 30)
    tui.wait(lambda s: _bbii_shell(s, FULL_DRAFT) and REPLY in s, "130 supported sidebar viewport")
    screen = step("history-opener", b"\x18h",
                  lambda s: (persisted := _bbii_persisted(ws))
                  and _bbii_history(s, tui.screen.cursor.hidden, FULL_DRAFT, title=persisted["history_title"]))
    persisted = _bbii_persisted(ws)
    assert persisted
    title = persisted["result_title"]
    (tui.evidence / "bb1b-ii-persisted.json").write_text(json.dumps(persisted, indent=2)+"\n")
    step("cross-unfiltered", b"/", cross())
    step("cross-empty", b"zzzz", cross("zzzz"))
    step("cross-backspace", b"\x7f", cross("zzz"))
    step("cross-clear", b"\x15", cross())
    queried = step("cross-filtered", b"Bb1a", cross("Bb1a", "1/1 matches", title))
    owner = cross("Bb1a", "1/1 matches", title)(queried)
    step("reserved-navigation", b"jk", lambda s: cross("Bb1a", "1/1 matches", title)(s) == owner)
    step("paste-edits-query", b"\x1b[200~blocked\x1b[201~",
         cross("Bb1ablocked"))
    step("restore-query-after-paste", b"\x15Bb1a", cross("Bb1a", "1/1 matches", title))
    step("cross-quit", b"\x11",
         lambda s: _bbii_quit(s, tui.screen.cursor.hidden, FULL_DRAFT, "History › Cross-Search › System · Quit"))
    step("cross-quit-cancel", b"n", lambda s: cross("Bb1a", "1/1 matches", title)(s) == owner)
    for label, key in (("esc", b"\x1b"), ("ctrl-c", b"\x03")):
        step("history-return-"+label, key, lambda s: _bbii_history(s, tui.screen.cursor.hidden, FULL_DRAFT, title=persisted["history_title"]))
        step("fresh-unfiltered-"+label, b"/", cross())
        step("same-query-"+label, b"Bb1a", cross("Bb1a", "1/1 matches", title))
    for width, height in ((80, 24), (60, 16)):
        tui.resize(width, height)
        screen = tui.wait(cross("Bb1a", "1/1 matches", title), f"{width} complete resized CrossSearch sheet")
        tui.capture(f"bb1b-ii-cross-{width}x{height}-full", screen)
    screen = step("enter-kept-match", b"\r", lambda s: _bbii_base(s, tui.screen.cursor.hidden, FULL_DRAFT)
                  and "Bb1a-row-0" in s and REPLY not in s)
    assert "Cross-Search" not in screen
    tui.exit(FULL_DRAFT)



@pytest.mark.parametrize("evidence_override", (None, "external-evidence", "smoke-evidence-4c"))
def test_scratch_paths_are_portable_and_socket_bounded(monkeypatch, evidence_override):
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
            if evidence_override is not None:
                override = root / evidence_override
                monkeypatch.setenv("RUSTAIN_CAPTURE_EVIDENCE_DIR", str(override))
                expected_parent = override.resolve()
            else:
                expected_parent = evidence.resolve()
            assert module["_evidence_parent"]() == expected_parent
            assert module["SCRATCH"] == scratch, "evidence override must not relocate sockets"
            assert module["_smoke_paths"]("bb1a") == (scratch, expected_parent)
            expected_ii = (evidence / "smoke-evidence-4bb1b-ii"
                           if evidence_override is None else expected_parent)
            assert module["_smoke_paths"]("bb1b-ii") == (scratch, expected_ii)
            c_scratch, c_parent = module["_smoke_paths"]("c")
            assert c_scratch == scratch, "C must use the same socket-bounded workspace scratch"
            c_expected = (expected_parent if expected_parent.name == "smoke-evidence-4c"
                          else expected_parent / "smoke-evidence-4c")
            assert c_parent == c_expected, "C must preserve portable evidence overrides"
            c_parent.mkdir(parents=True, exist_ok=True)
            with tempfile.TemporaryDirectory(prefix="r214-", dir=c_scratch) as workspace:
                for tag in ("l", "a"):
                    ws = Path(workspace) / tag
                    (ws / "data/daemons").mkdir(parents=True)
                    assert len(os.fsencode(ws / "data/daemons" / ("0"*16+".sock"))) < 108
            monkeypatch.delenv("RUSTAIN_CAPTURE_EVIDENCE_DIR", raising=False)


def _bb2b_box(screen, hidden, draft, kind):
    """Complete fitted inspector sheet; bottom-border list hints may vary."""
    shell = _bbii_shell(screen, draft)
    if not shell or not hidden:
        return None
    rows, region = shell
    width = len(rows[0])
    w, h = ((width, region) if width < 100 else
            (width*9//10, min(region, max(5, region*85//100))))
    left, top = (width-w)//2, (region-h)//2
    box = tuple(row[left:left+w] for row in rows[top:top+h])
    title = LOG if kind == "log" else HELP
    prefix = "┌ "+title+" "
    if (len(box) < 5 or box[0] != prefix+"─"*(w-len(prefix)-1)+"┐"
            or not (box[-1].startswith("└") and box[-1].endswith("┘"))
            or any(not (r.startswith("│") and r.endswith("│")) for r in box[1:-1])):
        return None
    return box


def _bb2b_inspector(screen, hidden, draft, kind, query, position, editing,
                    cells, mode="some", token=None):
    box = _bb2b_box(screen, hidden, draft, kind)
    if not box or box[1][1:-1].rstrip() != ("/ "+query).rstrip():
        return None
    content = "\n".join(row[1:-1] for row in box[2:-1])
    rail = "\n".join(box[2:])
    if editing:
        if not all(t in rail for t in ("[Enter]", "Keep filter", "[Esc]", "Clear")):
            return None
    elif "[/] Search" not in "\n".join(box):
        return None
    empty = "No matching notifications." if kind == "log" else "No matching keybindings."
    if mode == "empty":
        if empty not in content:
            return None
    elif empty in content or not content.strip():
        return None
    if kind == "log" and mode != "empty":
        if not re.search(r"\b(?:Info|Success|Warning|Error)\s*·", content):
            return None
    if kind == "log" and not editing:
        if not all(t in rail for t in ("[Enter]", "Detail", "[c]", "Copy")):
            return None
    if mode == "filtered" and (not token or token.lower() not in content.lower()):
        return None
    rows, region = _bbii_shell(screen, draft)
    top, left = (region-len(box))//2, (len(rows[0])-len(box[0]))//2
    actual = {(x, y) for x, y in cells if y == top+1}
    expected = {(left+3+position, top+1)} if editing else set()
    return box if actual == expected else None


def _bb2b_list(screen, hidden, draft, kind, token=None):
    box = _bb2b_box(screen, hidden, draft, kind)
    if not box or box[1][1:-1].lstrip().startswith("/") or "[/] Search" not in "\n".join(box):
        return None
    content = "\n".join(box[1:-1])
    if kind == "log":
        if not re.search(r"\b(?:Info|Success|Warning|Error)\s*·", content):
            return None
        if not all(t in "\n".join(box) for t in ("[Enter]", "Detail", "[c]", "Copy")):
            return None
    elif not content.strip("│ \n"):
        return None
    return box if token is None or token in content else None


def _bb2b_cross(screen, hidden, draft, query, position, cells, count=None, title=None):
    box = _bbii_frame(screen, hidden, draft, "History › Cross-Search", " [Enter] open  [Esc] close ")
    if not box or not _bbii_cross(screen, hidden, draft, query, count, title):
        return None
    rows, region = _bbii_shell(screen, draft)
    top, left = (region-len(box))//2, (len(rows[0])-len(box[0]))//2
    return box if {(x, y) for x, y in cells if y == top+1} == {(left+3+position, top+1)} else None


def _deskcheck_bb2b_frames(directory):
    """Real frames are positive controls; query styles, rails and torn edges aren't optional."""
    records = [json.loads(p.read_text()) for p in Path(directory).glob("bb2b-*.json")]
    frames = {(r["width"], r["height"], r["full"], r["stage"]): r for r in records}
    cases = {}
    for kind, needle, token in (("log", "aone", "/aone"), ("help", "notification", "notification")):
        for suffix, query, position, editing, mode in [
            ("unfiltered", "", 0, True, "some"),
            ("filtered", needle, len(needle), True, "filtered"),
            ("caret", needle, 1, True, "filtered"),
            ("empty", needle[:1]+"x"+needle[1:], 2, True, "empty"),
            ("restored", needle, 1, True, "filtered"),
            ("accepted", needle, 1, False, "filtered"),
            ("cleared", "", 0, False, "list"),
        ]:
            cases[kind+"-"+suffix] = (kind, query, position, editing, mode, token)
    for suffix, query, position, count, title in [
        ("unfiltered", "", 0, None, None),
        ("filtered", "Bb1a", 4, "1/1 matches", TURN),
        ("caret", "Bb1a", 1, "1/1 matches", TURN),
        ("empty", "Bxb1a", 2, None, None),
        ("restored", "Bb1a", 1, "1/1 matches", TURN),
        ("counted", "Bb1a", 4, "20/20 matches", TURN),
    ]:
        cases["cross-"+suffix] = ("cross", query, position, count, title, None)
    assert len(records) == len(frames) == 6*len(cases), (len(records), frames.keys())
    for width, height in ((60, 16), (80, 24), (130, 30)):
        for full in (False, True):
            for kind in ("log", "help", "cross"):
                a, b = (frames[width, height, full, kind+"-"+s] for s in ("filtered", "caret"))
                assert a["screen"] == b["screen"], "caret motion alone changes no query/body/shell text"
                assert a["reversed_cells"] != b["reversed_cells"], "observable style-motion control"
            for stage, values in cases.items():
                r = frames[width, height, full, stage]
                kind = values[0]
                def predicate(screen, hidden=r["cursor_hidden"], cells=r["reversed_cells"]):
                    if kind == "cross":
                        _, query, position, count, title, _ = values
                        return _bb2b_cross(screen, hidden, r["draft"], query, position, cells, count, title)
                    _, query, position, editing, mode, token = values
                    if mode == "list":
                        return _bb2b_list(screen, hidden, r["draft"], kind, token if kind == "log" else None)
                    return _bb2b_inspector(screen, hidden, r["draft"], kind, query, position,
                                           editing, cells, mode, token)
                box = predicate(r["screen"])
                assert box, (stage, r)
                assert not predicate(r["screen"], hidden=False), (stage, "second hardware cursor")
                assert not _bb2b_box(r["screen"], True, r["draft"],
                                     "help" if kind == "log" else "log"), (stage, "wrong owner")
                if kind == "cross" or values[3] is True:
                    assert not predicate(r["screen"], cells=[]), (stage, "missing actual caret")
                rows = r["screen"].split("\n")
                top = next(y for y, row in enumerate(rows) if box[0] in row)
                for y in (top, top+1, top+len(box)-1, harness.composer_top(rows)-1, height-1):
                    torn = rows.copy(); torn[y] = " "*width
                    assert not predicate("\n".join(torn)), (stage, width, full, y)
                left = rows[top].index(box[0])
                if kind == "cross" or values[4] != "list":
                    wrong_query = rows.copy()
                    y, x = top+1, left+1
                    wrong_query[y] = wrong_query[y][:x]+"#"+wrong_query[y][x+1:]
                    assert not predicate("\n".join(wrong_query)), (stage, "wrong query marker")
                for y in range(top+1, top+len(box)-1):
                    for x in (left, left+len(box[0])-1):
                        torn = rows.copy(); torn[y] = torn[y][:x]+" "+torn[y][x+1:]
                        assert not predicate("\n".join(torn)), (stage, "torn side", x, y)
                # Remove actual body content but retain every footer rail and
                # every border. The predicate must not accept a footer-only
                # partial frame as a settled nonempty result.
                bodyless = rows.copy()
                body_start = 1 if kind != "cross" and values[4] == "list" else 2
                for y in range(top+body_start, top+len(box)-1):
                    row = bodyless[y][left:left+len(box[0])]
                    if kind == "log" and re.search(
                            r"\[[^]]+\]\s+(?:Move|Detail|Copy|Retry|Dismiss|Search|Close|Keep filter|Clear)", row):
                        continue
                    bodyless[y] = (bodyless[y][:left+1]
                                   + " "*(len(box[0])-2)
                                   + bodyless[y][left+len(box[0])-1:])
                assert not predicate("\n".join(bodyless)), (stage, "missing actual body")
                if kind != "cross" and values[3] is True:
                    for label in ("[Enter]", "Keep filter", "[Esc]", "Clear"):
                        assert not predicate(r["screen"].replace(label, " "*len(label))), (stage, label)
                if kind != "cross" and values[3] is False:
                    assert not predicate(r["screen"].replace("[/] Search", " "*10)), (stage, "missing Search rail")
                if kind == "log" and values[3] is False:
                    for label in ("Detail", "Copy"):
                        assert not predicate(r["screen"].replace(label, " "*len(label))), (stage, label)
                if "empty" in stage:
                    marker = "No matches found" if kind == "cross" else (
                        "No matching notifications." if kind == "log" else "No matching keybindings.")
                    assert not predicate(r["screen"].replace(marker, " "*len(marker))), stage
    print(f"BB2B_DESKCHECK_FRAMES={len(frames)}", flush=True)
    return len(frames)


def _bb2b_fields_leg(tui):
    attached = tui.tag == "attached"
    notices = ("mode", "plan") if attached else ("aone", "btwo")
    tui.wait(lambda s: _status(s) and not tui.screen.cursor.hidden, "Bb2b Input ready")
    for marker in notices:
        command = "/"+marker
        tui.send("Bb2b real notice command", (command+" ").encode())
        tui.wait(lambda s: _composer(s) == command, "Bb2b slash delimiter consumed")
        tui.send("Bb2b submit local notice", b"\r")
        tui.wait(lambda s: _notice_ready(s, command, attached), "Bb2b actual notice settled")
    tui.send("Bb2b retained draft", DRAFT.encode())
    tui.wait(lambda s: _composer(s) == DRAFT, "Bb2b draft settled")
    for kind in ("log", "help"):
        title = LOG if kind == "log" else HELP
        tui.send("Bb2b open "+kind, b"\x18"+(b"n" if kind == "log" else b"?"))
        original = tui.wait(lambda s: tui.hidden_draw_complete and
                            _bb2b_list(s, tui.screen.cursor.hidden, DRAFT, kind),
                            "Bb2b complete unfiltered "+kind)
        if kind == "log":
            tui.send("Bb2b select retained non-newest record", b"j")
            tui.wait(lambda s: tui.hidden_draw_complete and _selected(tui, notices) == notices[0],
                     "Bb2b selected id control")
        else:
            for offset in range(1, 4):
                before = _help_box(original)
                tui.send(f"Bb2b actual Help scroll control {offset}", b"j")
                original = tui.wait(
                    lambda s: tui.hidden_draw_complete and
                    _bb2b_list(s, tui.screen.cursor.hidden, DRAFT, kind) and
                    _help_box(s) != before and _help_box(s)[:-1] == before[1:],
                    f"Bb2b complete one-row Help scroll {offset}")
        before_help = _help_box(original) if kind == "help" else None
        needle = ("/"+notices[0] if attached else notices[0]) if kind == "log" else "notification"
        token = "/"+notices[0] if kind == "log" else "notification"
        def field(query, position, mode="some", editing=True):
            return lambda s: _bb2b_inspector(s, tui.screen.cursor.hidden, DRAFT, kind,
                                              query, position, editing, _reversed_cells(tui), mode, token)
        def step(label, data, predicate):
            tui.send("Bb2b "+kind+" "+label, data)
            screen = tui.wait(lambda s: predicate(s) and
                              (tui.hidden_draw_complete or not tui.screen.cursor.hidden),
                              "Bb2b settled "+kind+" "+label)
            tui.capture("bb2b-"+kind+"-"+label, screen)
            return screen
        step("unfiltered", b"/", field("", 0))
        step("filtered", b"\x1b[200~"+needle.encode()+b"\x1b[201~", field(needle, len(needle), "filtered"))
        step("caret", b"\x1b[H\x1b[C", field(needle, 1, "filtered"))
        step("empty", b"\x1b[200~x\x1b[201~", field(needle[:1]+"x"+needle[1:], 2, "empty"))
        step("restored", b"\x08", field(needle, 1, "filtered"))
        step("kill-tail", b"\x0b", field(needle[:1], 1))
        step("kill-prefix", b"\x15", field("", 0))
        step("refilter", b"\x1b[200~"+needle.encode()+b"\x1b[201~\x1b[H\x1b[C", field(needle, 1, "filtered"))
        step("keep-filter", b"\r", field(needle, 1, "filtered", False))
        step("reenter", b"/", field(needle, 1, "filtered"))
        step("quit-cover", b"\x11", lambda s: _box(s, title+" › System · Quit", "Stay") and _status(s))
        step("quit-return", b"n", field(needle, 1, "filtered"))
        step("clear-child", b"\x1b", lambda s:
             _bb2b_list(s, tui.screen.cursor.hidden, DRAFT, kind, token if kind == "log" else None) and
             (kind != "help" or _help_box(s) == before_help))
        if kind == "log":
            assert _selected(tui, notices) == notices[0], "matching selected record id survives filtering"
        else:
            assert _help_box("\n".join(tui.screen.display)) == before_help, "unfiltered Help scroll restored"
        step("back-frame", b"\x1b", lambda s: _bb1a_base(s, DRAFT) and not tui.screen.cursor.hidden)


def _bb2b_cross_leg(tui, ws):
    """Pasted content starts the actual local scan; caret-only input keeps its result."""
    tui.resize(130, 30)
    tui.wait(lambda s: _bbii_shell(s, DRAFT) and not tui.screen.cursor.hidden, "Bb2b supported History width")
    tui.send("Bb2b supported History opener", b"\x18h")
    tui.wait(lambda s: (p := _bbii_persisted(ws)) and
             _bbii_history(s, tui.screen.cursor.hidden, DRAFT, title=p["history_title"]),
             "Bb2b persisted real History")
    title = _bbii_persisted(ws)["result_title"]
    tui.send("Bb2b CrossSearch opener", b"/")
    tui.wait(lambda s: _bbii_cross(s, tui.screen.cursor.hidden, DRAFT, ""), "Bb2b CrossSearch owned")
    tui.resize(80, 24)
    def field(query, position, count=None, selected=None):
        return lambda s: tui.hidden_draw_complete and _bb2b_cross(
            s, tui.screen.cursor.hidden, DRAFT, query, position, _reversed_cells(tui), count, selected)
    def step(label, data, predicate):
        tui.send("Bb2b CrossSearch "+label, data)
        screen = tui.wait(predicate, "Bb2b settled CrossSearch "+label)
        tui.capture("bb2b-cross-"+label, screen)
    tui.wait(field("", 0), "Bb2b 80x24 CrossSearch query caret")
    step("pasted-scan", b"\x1b[200~Bb1a\x1b[201~", field("Bb1a", 4, "1/1 matches", title))
    step("caret", b"\x1b[H\x1b[C", field("Bb1a", 1, "1/1 matches", title))
    step("empty", b"\x1b[200~x\x1b[201~", field("Bxb1a", 2))
    step("restored", b"\x08", field("Bb1a", 1, "1/1 matches", title))
    step("short-query", b"\x0b", field("B", 1))
    step("clear-query", b"\x15", field("", 0))
    step("refilter", b"\x1b[200~Bb1a\x1b[201~\x1b[H\x1b[C", field("Bb1a", 1, "1/1 matches", title))
    step("enter-result", b"\r", lambda s: _bbii_base(s, tui.screen.cursor.hidden, DRAFT)
         and "Bb1a-row-0" in s and REPLY not in s)


def _c_plain_shell(screen):
    """Validate the real reader's shared-composer bottom title before reuse."""
    rows = screen.split("\n")
    if rows and re.fullmatch(r"└ read-only — can't send here ─*┘", rows[-1]):
        rows[-1] = "└"+"─"*(len(rows[-1])-2)+"┘"
    return "\n".join(rows)


def _c_base(screen, draft, position, cursor, visible):
    """Shared composer pixels and hardware caret, not just its trimmed text."""
    # The common shell trims text; exact row/caret checks below retain positions.
    shell = _bbii_shell(_c_plain_shell(screen), draft.strip())
    if not shell or not visible:
        return None
    rows, status = shell
    if (rows[-2] != "│"+draft+" "*(len(rows[0])-2-len(draft))+"│"
            or tuple(cursor) != (position+1, len(rows)-2)):
        return None
    return tuple(rows[:status])


def _c_view(screen, draft, cursor, visible, earlier):
    stage = _c_base(screen, draft, len(draft), cursor, visible)
    if not stage:
        return None
    text = "\n".join(stage)
    return stage if (("Bb1a-row-" in text and REPLY not in text) if earlier else REPLY in text) else None


def _c_cover(screen, draft, visible, kind, token):
    screen = _c_plain_shell(screen)
    rows = screen.split("\n")
    if len(rows) < 3 or rows[-2] != "│"+draft+" "*(len(rows[0])-2-len(draft))+"│":
        return None
    box = _bb2b_list(screen, not visible, draft, kind, token if kind == "log" else None)
    if not box:
        return None
    if kind == "help" and (not _help_box(screen) or
                           "Rustain is a composable" not in "\n".join(box[1:-1])):
        return None
    return box


def _deskcheck_c_frames(directory):
    """Only saved real production paints are positives; this path never spawns."""
    cases = {"initial": ("", 0), "notice": ("", 0), "notice-dismiss-return": ("", 0),
             "typed": ("C-kept draft", 12), "readline-home": ("C-kept draft", 0),
             "left": ("C-kept draft", 11), "caret": ("C-kept draft", 1),
             "paste": ("CX-kept draft", 2), "kill-tail": ("CX", 2), "kill-prefix": ("", 0)}
    common = set(cases) | {"notice-command", "bottom", "scrolled", "help", "help-scrolled",
                           "help-return", "log", "log-return"}
    def stages(host):
        extra = {"readonly-command", "readonly-log"} if host == "readonly" else {"notice-log"}
        return common | extra | ({"chat-before-scroll", "chat-scrolled"} if host == "local" else set())
    records = [json.loads(p.read_text()) for p in Path(directory).glob("c-*.json")]
    frames = {(r["width"], r["height"], r["host"], r["stage"]): r for r in records}
    required = {(w, h, host, stage) for w, h in ((60, 16), (80, 24), (130, 30))
                for host in ("local", "attached", "readonly") for stage in stages(host)}
    assert len(records) == len(frames) and frames.keys() == required, frames.keys() ^ required
    for width, height in ((60, 16), (80, 24), (130, 30)):
        for host in ("local", "attached", "readonly"):
            token = "/cnotice" if host == "local" else "/mode"
            def record(stage):
                r = frames[width, height, host, stage]
                assert r["full"] is False
                assert len(r["screen"].split("\n")) == height
                assert all(len(row) == width for row in r["screen"].split("\n"))
                assert ("read-only — can't send here" in r["screen"].split("\n")[-1]) == (host == "readonly")
                assert KEY not in json.dumps(r)
                assert all(0 <= x < width and 0 <= y < height for x, y in r["reversed_cells"])
                return r
            def visible(r):
                return r["cursor_visible"] if "cursor_visible" in r else not r["cursor_hidden"]
            before = record("initial")
            for stage in stages(host):
                r = record(stage)
                cover = stage in ("help", "help-scrolled", "log", "notice-log", "readonly-log")
                chat = stage in ("chat-before-scroll", "chat-scrolled")
                draft, position = cases.get(stage, (
                    "" if stage in ("notice-log", "readonly-log") else "C-kept draft", 12))
                if stage == "notice-command":
                    draft, position = token+" ", len(token)+1
                elif stage == "readonly-command":
                    draft, position = "/notifications ", 15
                assert r["draft"] == draft
                def predicate(screen, cursor=r["cursor"], shown=visible(r)):
                    if stage == "help-scrolled":
                        box = _bb2b_list(_c_plain_shell(screen), not shown, draft, "help")
                        shifted, prior = _help_box(screen), _help_box(record("help")["screen"])
                        return box if (box and shifted and prior and shifted != prior
                                       and shifted[:-1] == prior[1:]) else None
                    if cover:
                        return _c_cover(screen, draft, shown, "help" if stage == "help" else "log", token)
                    if chat:
                        shell = _bbii_shell(_c_plain_shell(screen), draft)
                        if not shell or shown:
                            return None
                        body = tuple(shell[0][:shell[1]])
                        earlier = "Bb1a-row-" in "\n".join(body) and REPLY not in "\n".join(body)
                        return body if (earlier if stage == "chat-scrolled" else REPLY in "\n".join(body)) else None
                    if stage == "notice" and not _notice_ready(screen, token, host != "local"):
                        return None
                    if stage == "notice-dismiss-return" and any(
                            token in row for row in harness.toast_rows(screen, rows=3)):
                        return None
                    if stage in ("bottom", "scrolled", "help-return", "log-return"):
                        return _c_view(screen, draft, cursor, shown, stage != "bottom")
                    return _c_base(screen, draft, position, cursor, shown)
                owned = predicate(r["screen"])
                assert owned, (host, stage, r)
                assert not predicate(r["screen"], shown=not visible(r)), (stage, "wrong hardware ownership")
                if not cover and not chat:
                    assert not predicate(r["screen"], cursor=[width-1, height-1]), (stage, "wrong caret")
                    if stage != "initial" and draft:
                        assert not predicate(before["screen"]), (stage, "independent empty composer")
                elif cover:
                    assert not predicate(before["screen"]), (stage, "independent no-owner frame")
                rows = r["screen"].split("\n")
                torn_rows = {harness.composer_top(rows), height-4, height-2, height-1}
                if cover:
                    top = next(y for y, row in enumerate(rows) if owned[0] in row)
                    left = rows[top].index(owned[0])
                    torn_rows |= {top, top+len(owned)-1}
                    for y in range(top+1, top+len(owned)-1):
                        for x in (left, left+len(owned[0])-1):
                            torn = rows.copy(); torn[y] = torn[y][:x]+" "+torn[y][x+1:]
                            assert not predicate("\n".join(torn)), (stage, "torn side", x, y)
                    labels = ("j/k scroll", "? close", "[/] Search") if stage.startswith("help") else (
                        "[Enter]", "Detail", "[c]", "Copy", "[/] Search")
                    for label in labels:
                        assert not predicate(r["screen"].replace(label, " "*len(label))), (stage, label)
                    if stage == "help-scrolled":
                        assert not predicate(record("help")["screen"]), (stage, "observable real scroll")
                    else:
                        marker = "Rustain is a composable" if stage == "help" else token
                        assert not predicate(r["screen"].replace(marker, " "*len(marker))), (stage, "body-only control")
                if stage == "notice":
                    assert not predicate(before["screen"]), (stage, "real toast required")
                    assert not predicate(r["screen"].replace(token, " "*len(token))), (stage, "notice body")
                    for corrupt in (token[:-1]+" ", token[:-1]+"x"):
                        assert not predicate(r["screen"].replace(token, corrupt)), (stage, "complete notice token")
                if stage in ("notice-command", "readonly-command"):
                    assert not predicate(r["screen"], cursor=[position, height-2]), (stage, "trailing-space caret")
                for y in torn_rows:
                    torn = rows.copy(); torn[y] = " "*width
                    assert not predicate("\n".join(torn)), (host, stage, "torn row", y)
                if stage in ("bottom", "scrolled", "help-return", "log-return") or chat:
                    marker = REPLY if stage in ("bottom", "chat-before-scroll") else "Bb1a-row-"
                    assert not predicate(r["screen"].replace(marker, " "*len(marker))), (stage, "transcript body")
            typed, caret = record("typed"), record("caret")
            assert typed["screen"] == caret["screen"] and typed["cursor"] != caret["cursor"]
            bottom, scrolled = record("bottom"), record("scrolled")
            assert not _c_view(bottom["screen"], bottom["draft"], bottom["cursor"], visible(bottom), True)
            assert not _c_view(scrolled["screen"], scrolled["draft"], scrolled["cursor"], visible(scrolled), False)
            original = _c_view(scrolled["screen"], scrolled["draft"], scrolled["cursor"], visible(scrolled), True)
            for stage in ("help-return", "log-return"):
                r = record(stage)
                assert _c_view(r["screen"], r["draft"], r["cursor"], visible(r), True) == original
    print(f"C_DESKCHECK_FRAMES={len(frames)}", flush=True)
    return len(frames)


def _c_leg(tui):
    """Same editor and authoritative scrolled view in local, writer and reader."""
    def base(draft, position):
        return lambda s: _c_base(s, draft, position,
                                 (tui.screen.cursor.x, tui.screen.cursor.y), not tui.screen.cursor.hidden)
    def step(stage, data, predicate):
        tui.send("C "+stage, data)
        screen = tui.wait(predicate, "C settled "+stage)
        tui.capture("c-"+stage, screen)
        return screen
    screen = tui.wait(base("", 0), "C starts in Input without i")
    tui.capture("c-initial", screen)
    attached = tui.tag != "local"
    command = "/mode" if attached else "/cnotice"
    step("notice-command", (command+" ").encode(), base(command+" ", len(command)+1))
    step("notice", b"\r", lambda s: _notice_ready(s, command, attached) and base("", 0)(s))
    # Dismiss just the toast presentation through the real retained Log record,
    # removing TTL changes from the later exact transcript-pixel comparison.
    if tui.tag == "readonly":
        step("readonly-command", b"/notifications ", base("/notifications ", 15))
        opener, stage = b"\r", "readonly-log"
    else:
        opener, stage = b"\x18n", "notice-log"
    step(stage, opener, lambda s: tui.hidden_draw_complete and
         _c_cover(s, "", not tui.screen.cursor.hidden, "log", command))
    step("notice-dismiss-return", b"x\x1b", lambda s: base("", 0)(s) and
         not any(command in row for row in harness.toast_rows(s, rows=3)))
    typed = step("typed", b"C-kept draft", base("C-kept draft", 12))
    for label, data, position in (("readline-home", b"\x01", 0), ("readline-end", b"\x05", 12),
                                  ("left", b"\x1b[D", 11), ("end", b"\x1b[F", 12)):
        assert step(label, data, base("C-kept draft", position)) == typed
    caret = step("caret", b"\x1b[H\x1b[C", base("C-kept draft", 1))
    assert typed == caret, "caret motion must not mutate draft or transcript pixels"
    step("paste", b"\x1b[200~X\x1b[201~", base("CX-kept draft", 2))
    step("readline-backspace", b"\x08", base("C-kept draft", 1))
    step("repaste", b"\x1b[200~X\x1b[201~", base("CX-kept draft", 2))
    step("kill-tail", b"\x0b", base("CX", 2))
    step("kill-prefix", b"\x15", base("", 0))
    step("word-fixture", b"CX", base("CX", 2))
    step("kill-word", b"\x17", base("", 0))
    draft = "C-kept draft"
    def view(earlier):
        return lambda s: _c_view(s, draft, (tui.screen.cursor.x, tui.screen.cursor.y),
                                 not tui.screen.cursor.hidden, earlier)
    step("bottom", draft.encode(), view(False))
    if tui.tag == "local":
        # Local full-page up is Chat Ctrl+B; attached Input offers PgUp.
        step("chat-before-scroll", b"\x1b", lambda s: tui.hidden_draw_complete and
             _bbii_shell(s, draft) and tui.screen.cursor.hidden and REPLY in s)
        step("chat-scrolled", b"\x02", lambda s: tui.hidden_draw_complete and
             _bbii_shell(s, draft) and tui.screen.cursor.hidden and
             "Bb1a-row-" in s and REPLY not in s)
        original = step("scrolled", b"i", view(True))
    else:
        original = step("scrolled", b"\x1b[5~", view(True))
    original_stage = view(True)(original)
    for kind, key in (("help", b"?"), ("log", b"n")):
        covered = step(kind, b"\x18"+key, lambda s: tui.hidden_draw_complete and
                       _c_cover(s, draft, not tui.screen.cursor.hidden, kind, command))
        if kind == "help":
            tui.send("C observable covered Help scroll", b"j")
            tui.wait(lambda s: tui.hidden_draw_complete and
                     _bb2b_list(_c_plain_shell(s), tui.screen.cursor.hidden, draft, kind) and
                     _help_box(s) != _help_box(covered) and _help_box(s)[:-1] == _help_box(covered)[1:],
                     "C complete changed Help body without transcript movement")
        step(kind+"-return", b"\x1b", lambda s: view(True)(s) == original_stage)


def _run_modal_stack_smoke(part):
    assert part in ("bb1a", "bb1b-i", "bb1b-ii", "bb2b", "c")
    local_only = part not in ("bb1a", "bb2b", "c")
    turns = 2 if part == "c" else (3 if part == "bb1b-i" else 1)
    part_name = part.replace("bb", "Bb", 1)
    if os.environ.get("RUSTAIN_MODAL_STACK_DESKCHECK") == "1":
        if part == "c":
            _deskcheck_c_frames(os.environ["RUSTAIN_MODAL_STACK_C_FRAMES"])
            return
        if part == "bb2b":
            _deskcheck_bb2b_frames(os.environ["RUSTAIN_MODAL_STACK_BB2B_FRAMES"])
            return
        frames_ii = os.environ.get("RUSTAIN_MODAL_STACK_BB1B_II_FRAMES")
        if part == "bb1b-ii" or (frames_ii and not os.environ.get("RUSTAIN_MODAL_STACK_BB1B_I_FRAMES")):
            _deskcheck_bb1b_ii_frames(frames_ii)
        elif part == "bb1b-i":
            _deskcheck_bb1b_i_frames(os.environ["RUSTAIN_MODAL_STACK_BB1B_I_FRAMES"])
        else:
            _deskcheck()
        return
    binary = harness._resolve_binary()[0].resolve()
    assert binary.is_file() and "debug" in binary.parts and "release" not in binary.parts, binary
    scratch, parent = _smoke_paths(part)
    parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix=f"21-4-{part}-", dir=parent))
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    manifest = {"binary": str(binary), "sha256": digest, "bytes": binary.stat().st_size, "viewport": [80, 24],
                "started_at": datetime.now(timezone.utc).isoformat(), "result": "RUNNING",
                "legs": ["local", "attached"], "story_parts": ["A1", "A2a", "A2b", "Ba", "Bb1a", "Bb2a"]}
    if part == "bb2b":
        manifest.update(story_parts=["Bb2b"], cross_search="local only; attached unavailable")
    if part == "c":
        manifest.update(story_parts=["C"], legs=["local", "attached", "readonly"],
                        attached_unavailable="Model/Profile/Login/Usage/Search/Bookmark/Wave/CrossSearch n/a")
    if local_only:
        surface = "Bookmark/Wave" if part == "bb1b-i" else "CrossSearch"
        manifest.update(legs=["local"], story_parts=[part_name],
                        attached=f"{surface} n/a: local-only")
    if part == "bb1b-ii":
        manifest["viewports"] = [[80, 24], [130, 30], [80, 24], [60, 16]]
    errors, posts = queue.Queue(), []
    held, release = threading.Event(), threading.Event()
    request_lock = threading.Lock()
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
                    self.reply(b"{}" if self.path.split("?", 1)[0] == "/api.json" else b'{"data":[{"id":"claude-sonnet-4-6","type":"model","display_name":"Claude Sonnet 4.6","created_at":"2025-01-01T00:00:00Z"}],"has_more":false}')
                def do_POST(self):
                    try:
                        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                        probe = body.get("max_tokens") == 1 and "stream" not in body
                        with request_lock:
                            posts.append({"kind": "probe" if probe else "turn",
                                          "max_tokens": body.get("max_tokens"), "stream": body.get("stream", False)})
                            turn = sum(post["kind"] == "turn" for post in posts)
                        if probe:
                            self.reply(b'{"type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}')
                            return
                        assert body.get("stream") is True and TURN in json.dumps(body.get("messages", []))
                        assert turn <= turns, "unexpected extra real turn"
                        reply = SEARCH_REPLY if turn == 1 or part == "c" else NOTICES[turn-2]
                        if turn == 1:
                            held.set()
                            assert release.wait(90), "held model turn was never released"
                        events = [
                            {"type": "message_start", "message": {"usage": {"input_tokens": 5}}},
                            {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}},
                            {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": reply}},
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
            scratch.mkdir(parents=True, exist_ok=True)
            root = Path(stack.enter_context(tempfile.TemporaryDirectory(prefix="r214-", dir=scratch)))
            (root / "home").mkdir()
            origin = f"http://127.0.0.1:{server.server_port}"
            def environment(tag, ws):
                config = ws / ".rustain"; config.mkdir(parents=True)
                settings = "[notifications]\ninfo_ttl_secs = 30\n"
                if tag == "local" and part == "bb1a":
                    settings += ('\n[budget]\ndaily_limit_usd = 0.000001\n'
                                 '\n[pricing."claude-sonnet-4-6"]\ninput_per_million = 3.0\noutput_per_million = 15.0\n')
                (ws / ".env").write_text(""); (config / "config.toml").write_text(settings)
                return {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LANG": "C.UTF-8", "TERM": "xterm-256color",
                        "HOME": str(root / "home"), "XDG_CONFIG_HOME": str(root / "config"), "XDG_DATA_HOME": str(root / "xdg-data"),
                        "XDG_CACHE_HOME": str(root / "cache"), "RUSTAIN_CONFIG_DIR": str(config), "RUSTAIN_DATA_DIR": str(ws / "data"),
                        "RUSTAIN_LOG_PATH": str(evidence / f"{tag}.runtime.log"), "RUST_LOG": "debug",
                        "ANTHROPIC_API_KEY": KEY, "ANTHROPIC_BASE_URL": origin, "RUSTAIN_MODELS_DEV_URL": origin, "NO_PROXY": "*"}
            for tag in (("local",) if local_only else ("local", "attached")):
                ws = root / ("l" if tag == "local" else "a"); env = environment(tag, ws)
                assert len(os.fsencode(ws / "data/daemons" / ("0"*16+".sock"))) < 108
                if tag == "attached":
                    daemon_env = {**env, "RUSTAIN_LOG_PATH": str(evidence / "daemon.runtime.log")}
                    _daemon(stack, binary, ws, daemon_env, evidence)
                tui = _Terminal(stack, binary, ["--new"] if tag == "local" else ["daemon", "attach"], ws, env, evidence, tag, errors)
                if part == "c":
                    release.set()
                    tui.wait(lambda s: _c_base(s, "", 0, (tui.screen.cursor.x, tui.screen.cursor.y),
                                              not tui.screen.cursor.hidden), "C initial shared Input")
                    tui.send("C real transcript turn", TURN.encode()+b"\r")
                    tui.wait(lambda s: _status(s) and REPLY in s and not tui.screen.cursor.hidden,
                             "C real transcript completion")
                    _c_leg(tui)
                    if tag == "attached":
                        reader_env = {**env, "RUSTAIN_LOG_PATH": str(evidence / "readonly.runtime.log")}
                        reader = _Terminal(stack, binary, ["daemon", "attach"], ws, reader_env,
                                           evidence, "readonly", errors)
                        reader.wait(lambda s: _status(s) and "read-only" in s and REPLY in s,
                                    "C real daemon-granted read-only snapshot")
                        _c_leg(reader)
                        reader.exit()
                    tui.exit()
                    continue
                if local_only:
                    release.set()
                    tui.wait(lambda s: _status(s) and not tui.screen.cursor.hidden, f"{part_name} initial Input")
                    tui.send(f"{part_name} real fixture turn", TURN.encode()+b"\r")
                    tui.wait(lambda s: held.is_set() and _status(s) and REPLY in s, f"{part_name} real fixture completion")
                    if part == "bb1b-i":
                        tui.send("Bb1b-i retained draft", DRAFT.encode())
                        tui.wait(lambda s: _composer(s) == DRAFT, "Bb1b-i fixture draft")
                        _bb1b_i_leg(tui)
                        tui.exit()
                    else:
                        _bb1b_ii_leg(tui, ws)
                elif part == "bb2b":
                    if tag == "local":
                        release.set()
                        tui.wait(lambda s: _status(s) and not tui.screen.cursor.hidden, "Bb2b initial Input")
                        tui.send("Bb2b real persisted fixture turn", TURN.encode()+b"\r")
                        tui.wait(lambda s: held.is_set() and _status(s) and REPLY in s,
                                 "Bb2b real fixture completion")
                    _bb2b_fields_leg(tui)
                    if tag == "local":
                        _bb2b_cross_leg(tui, ws)
                    tui.exit()
                else:
                    _leg(tui, held, release)
        assert errors.empty(), list(errors.queue)
        assert held.is_set() and release.is_set()
        assert sum(post["kind"] == "turn" for post in posts) == turns, posts
        logs = sorted(evidence.glob("*.runtime.log.*"))
        for tag in (("local",) if local_only else ("local", "attached", "daemon")):
            assert any(p.name.startswith(tag+".runtime.log.") and re.fullmatch(r"\d{4}-\d{2}-\d{2}", p.suffix[1:]) for p in logs), f"missing dated {tag} debug log"
        if part == "c":
            assert any(p.name.startswith("readonly.runtime.log.") for p in logs), "missing reader debug log"
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
        label = "NATIVE_21_4_"+part.upper().replace("-", "_")+"_EVIDENCE"
        print(f"{label}={evidence}", flush=True)


@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(240)
def test_modal_stack_a1_a2a_local_and_attached_80x24():
    _run_modal_stack_smoke("bb1a")


@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(240)
def test_modal_stack_bb1b_i_local_80x24():
    _run_modal_stack_smoke("bb1b-i")


@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(240)
def test_modal_stack_bb1b_ii_local_cross_search():
    _run_modal_stack_smoke("bb1b-ii")


@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(240)
def test_modal_stack_bb2b_inspector_fields_both_hosts_and_cross_query():
    _run_modal_stack_smoke("bb2b")


@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(360)
def test_modal_stack_c_shared_composer_and_view_both_hosts(monkeypatch):
    if os.environ.get("RUSTAIN_MODAL_STACK_DESKCHECK") == "1":
        def forbidden(*_args, **_kwargs):
            raise AssertionError("C DESKCHECK must not resolve or spawn an app")
        monkeypatch.setattr(harness, "_resolve_binary", forbidden)
        monkeypatch.setattr(subprocess, "Popen", forbidden)
        monkeypatch.setattr(pexpect, "spawn", forbidden)
    _run_modal_stack_smoke("c")
