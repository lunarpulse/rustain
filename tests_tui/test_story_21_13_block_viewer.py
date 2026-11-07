"""21-13 native smoke (slices 13-A and 13-B): the Stage `[Enter] Inspect` cue, the
capped Stage and the S-BLOCK window on a real debug binary at 80x24 (scene
provider, one spawn).

Leg A: reply with a 45-line rust block (line 7 is 120 cells, ending `TAIL120`) and
an 8-column table. `Esc` -> Chat, `gg` -> the block header row carries `▸` and the
cue; `Enter` -> the `Block · rust #1` window; `$` shows the tail of line 7 and no
`→`; `0` returns; `G`, `c` (clipboard file), `Esc` back to the same Stage rows.
Leg B: `G` shows the cap's `… 15 more lines` row inside the box; `Enter`, `#`, `G`
line 45 with its number, `]` the table neighbour, `v` its markdown source, `Esc`.

Offline first: RUSTAIN_BLOCK_VIEWER_DESKCHECK=1 with RUSTAIN_BLOCK_VIEWER_FRAMES=<dir>
consumes only saved Rust paints (see conformance_21_13 `smoke_frames`); no binary,
subprocess or scene provider is started. Predicates are anchored on whole rows.
"""
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import sys
import tempfile
import traceback

import harness
import pytest

sys.path.insert(0, str(Path(__file__).parent))

from fixtures.scene_provider import SceneStub  # noqa: E402
from test_story_20_1c_locked_providers import (  # noqa: E402
    PROVIDER_ENV, _NativeTUI, _capture, _sensitive_env, _sha256, _wait, status_ready,
)

SCENE = Path(__file__).parent / "fixtures" / "scenes" / "block_viewer.json"
PERSONA = "scene-ci-block-viewer"
DESKCHECK = os.environ.get("RUSTAIN_BLOCK_VIEWER_DESKCHECK") == "1"


def _rows(screen):
    return [row.rstrip() for row in screen.split("\n")]


def _matches_row(screen, pattern):
    compiled = re.compile(pattern)
    return any(compiled.fullmatch(row) for row in _rows(screen))


def _ready(screen):
    rows = screen.split("\n")
    tops = [y for y, row in enumerate(rows) if row.startswith("┌")]
    return bool(tops) and tops[-1] > 0 and status_ready(rows[tops[-1] - 1])


MARKER = r"▸ ┌─ rust ─+ \[Enter\] Inspect ─┐"
STATUS = r"[│ ]*code 1/2 · rust · 45 lines · Rendered · col (\d+)-(\d+)/122[│ ]*"


def _stage(screen):
    return _ready(screen) and _matches_row(screen, r"│ ┌─ rust ─+┐|│ │fn line_\d\d\(\) \{\}.*")


def _marked(screen):
    return _ready(screen) and _matches_row(screen, MARKER) and sum(
        1 for row in _rows(screen) if "▸" in row) == 1


def _opened(screen):
    rows = _rows(screen)
    return (any("Block · rust #1" in row for row in rows)
            and _matches_row(screen, STATUS)
            and _matches_row(screen, r"│┌─ rust ─+→│")
            and any("[Esc] Back" in row for row in rows))


def _tail(screen):
    return (_matches_row(screen, r"[│ ]*←.*TAIL120\";│[│ ]*|.*TAIL120\";│[│ ]*")
            and not any(row.rstrip("│ ").endswith("→") for row in _rows(screen))
            and _matches_row(screen, r"[│ ]*code 1/2 · rust · 45 lines · Rendered · col \d+-122/122[│ ]*"))


def _start(screen):
    m = re.search(r"col 1-(\d+)/122", "\n".join(_rows(screen)))
    return m is not None and _marked_window(screen)


def _marked_window(screen):
    return any("Block · rust #1" in row for row in _rows(screen))


def _stage_rows(screen):
    """Every Stage row: all but the status row and the composer box."""
    return _rows(screen)[:-4]


TOAST = re.compile(r"(Warning|Info|Success|Error) [A-Z]")


def _toast_rows(screen):
    return [row for row in _stage_rows(screen) if TOAST.search(row)]


def _quiet_marked(screen):
    """The marked Stage with no expiring toast on it: a stable baseline."""
    return _marked(screen) and not _toast_rows(screen)


def _end(screen):
    return (any("Block · rust #1" in row for row in _rows(screen))
            and any("// end-45" in row for row in _rows(screen)))


CAP = r"│ │… 15 more lines +│"
# Narrow sheets cut the column range with an explicit ellipsis; wider ones show it
# whole or not at all (a window as wide as the block has nothing to scroll).
STATUS_NUMBERS = (r"[│ ]*code 1/2 · rust · 45 lines · Rendered · numbers"
                  r"( · col [\d/…-]+)?[│ ]*")


def _capped(screen):
    """The Stage with the cap's remainder row inside the code box."""
    return _ready(screen) and _matches_row(screen, CAP)


def _numbers_end(screen):
    return (any("Block · rust #1" in row for row in _rows(screen))
            and any("45 │ │// end-45" in row for row in _rows(screen)))


def _next_table(screen):
    return (any("Block · table #2" in row for row in _rows(screen))
            and _matches_row(screen, r"[│ ]*table 2/2 · 8 cols × 3 rows · Rendered[│ ]*"))


def _source_table(screen):
    return (any("Block · table #2" in row for row in _rows(screen))
            and _matches_row(screen, r"[│ ]*table 2/2 · 8 cols × 3 rows · Source · numbers"
                                     r"( · col [\d/…-]+)?[│ ]*")
            and _matches_row(screen, r"[│ ]*1 │ \| c1 \| c2 \| c3 .*\|[│ ]*"))


def _interior():
    text = json.loads(SCENE.read_text())["personas"][PERSONA]["turns"][0]["content"][0]["text"]
    return text.split("```rust\n", 1)[1].split("\n```", 1)[0]


def _back(screen, stage_rows=None):
    return (_marked(screen) and not any("Block ·" in row for row in _rows(screen))
            and (stage_rows is None or _stage_rows(screen) == stage_rows))


def _frame(frames, size, name):
    return json.loads((frames / f"{size}-{name}.json").read_text())["screen"]


def _window_any_size(screen):
    """Title, status row (col range only when something is wider) and the Back rail."""
    rows = _rows(screen)
    return (any("Block · rust #1" in row for row in rows)
            and _matches_row(screen, r"[│ ]*code 1/2 · rust · 45 lines · Rendered( · col \d+-\d+/122)?[│ ]*")
            and any("[Esc] Back" in row for row in rows))


def _deskcheck():
    frames = Path(os.environ["RUSTAIN_BLOCK_VIEWER_FRAMES"])
    # 80x24: the smoke's own predicates accept the real paints and reject the others.
    stage, opened, tail = (_frame(frames, "80x24", name) for name in ("marked", "opened", "tail"))
    end = _frame(frames, "80x24", "end")
    assert _marked(stage) and not _marked(opened.replace("▸", " "))
    assert _opened(opened) and _tail(tail) and _end(end)
    assert not _tail(opened) and not _opened(stage) and not _end(opened)
    # Torn frames (a cut row, a missing rail, a half-painted title) are rejected.
    assert not _marked(stage.replace("Inspect ─┐", "Insp"))
    assert not _opened(opened.replace("[Esc] Back", "[Esc] Ba"))
    assert not _opened(opened.replace("Block · rust #1", "Block · ru"))
    # A toast on a Stage row makes the baseline unstable: marked but not quiet.
    rows = stage.split("\n")
    rows[18] = rows[18][:20] + "Warning Model discovery for 'anthropic' returned 0 models."
    toasted = "\n".join(rows)
    assert _marked(toasted) and not _quiet_marked(toasted) and _quiet_marked(stage)
    failing = os.environ.get("RUSTAIN_BLOCK_VIEWER_FAILING_FRAME")
    if failing:  # the saved real frame of the quarantined attempt
        saved = Path(failing).read_text().rstrip("\n")
        assert _marked(saved) and not _quiet_marked(saved)
    # 60x16 and 130x30 saved S-BLOCK paints: the window predicate holds, and each
    # size shows its own end-of-body row (the 1-row body shows the bottom border).
    for size in ("60x16", "130x30"):
        assert _window_any_size(_frame(frames, size, "opened")), size
        assert not _window_any_size(_frame(frames, size, "marked")), size
        assert not _window_any_size(_frame(frames, size, "opened").replace("[Esc] Back", "")), size
    assert _matches_row(_frame(frames, "60x16", "tail"), r"[│ ]*code 1/2 · rust · 45 lines · Rendered · col \d+-122/122[│ ]*")
    assert "col" not in _frame(frames, "130x30", "tail").split("\n")[1]
    assert _end(_frame(frames, "130x30", "end"))
    assert any(row.lstrip("│ ").startswith("└") for row in _rows(_frame(frames, "60x16", "end")))
    # Leg B frames at every size. The capped Stage shows its remainder row; numbers,
    # the neighbour and the source are generic window predicates, and each size shows
    # its own end-of-body row (a 1-row body shows the bottom border, blank gutter).
    for size in ("80x24", "60x16", "130x30"):
        bottom = _frame(frames, size, "stage-bottom")
        numbers_end = _frame(frames, size, "numbers-end")
        nxt, source = _frame(frames, size, "next"), _frame(frames, size, "source")
        assert _capped(bottom) and not _capped(_frame(frames, size, "marked")), size
        assert _matches_row(numbers_end, STATUS_NUMBERS), size
        assert _next_table(nxt) and not _next_table(source), size
        assert _source_table(source) and not _source_table(nxt), size
        assert not _capped(bottom.replace("more lines", "more line")), size
        assert not _next_table(nxt.replace("Rendered", "Rend")), size
        assert not _source_table(source.replace("· Source ·", "· Sour")), size
    for size in ("80x24", "130x30"):
        assert _numbers_end(_frame(frames, size, "numbers-end")), size
    assert any(re.fullmatch(r"│? *│ └─+→?│?", row) for row in _rows(_frame(frames, "60x16", "numbers-end")))
    assert not _numbers_end(_frame(frames, "80x24", "end"))


@pytest.fixture(scope="session", autouse=True)
def build_binary():
    """Offline desk-check starts no binary; live runs use the default resolution."""
    if not DESKCHECK:
        assert harness._resolve_binary()[0].is_file()


@pytest.mark.skipif(not DESKCHECK, reason="offline desk-check only")
def test_block_viewer_predicates_accept_real_saved_frames_and_reject_torn_ones():
    _deskcheck()


@pytest.mark.skipif(DESKCHECK, reason="live run is skipped during the offline desk-check")
@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(600)
def test_block_viewer_cue_window_and_horizontal_scroll_at_80x24(monkeypatch):
    parent = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())).resolve()
    parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="21-13-", dir=parent))
    manifest = {"run_id": evidence.name, "started_at": datetime.now(timezone.utc).isoformat(),
                "evidence_dir": str(evidence), "viewport": [80, 24], "result": "RUNNING"}
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    original_spawn = harness.pexpect.spawn
    try:
        binary = harness._resolve_binary()[0].resolve()
        assert binary.is_file() and "debug" in binary.parts and "release" not in binary.parts, binary
        manifest.update(binary=str(binary), binary_sha256=_sha256(binary),
                        profile="debug", features="default")
        (evidence / "run.json").write_text(json.dumps(manifest, indent=2) + "\n")
        with tempfile.TemporaryDirectory(prefix="r21-13-") as root_name:
            _spawn(binary, evidence, Path(root_name), original_spawn, monkeypatch)
        assert _sha256(binary) == manifest["binary_sha256"], "binary changed during native smoke"
        secrets = {value for key, value in harness._load_env().items()
                   if _sensitive_env(key) and key != "NO_PROXY" and len(value) >= 12}
        for artifact in sorted(evidence.iterdir()):
            if artifact.is_file():
                text = artifact.read_text(encoding="utf-8", errors="replace")
                assert not any(secret in text for secret in secrets), artifact.name
        manifest["result"] = "PASS"
    except BaseException:
        manifest["result"] = "FAIL"
        (evidence / "failure.txt").write_text(traceback.format_exc())
        raise
    finally:
        manifest["completed_at"] = datetime.now(timezone.utc).isoformat()
        (evidence / "run.json").write_text(json.dumps(manifest, indent=2) + "\n")
        print(f"NATIVE_21_13_EVIDENCE={evidence}", flush=True)


def _spawn(binary, evidence, root, original_spawn, monkeypatch):
    workspace = root / "fresh-cwd"
    workspace.mkdir()
    (workspace / ".env").write_text("")
    home = root / "home"
    config, data = home / "config", home / "data"
    config.mkdir(parents=True)
    data.mkdir()
    stub = SceneStub(SCENE, evidence / "a.requests.jsonl").start()
    try:
        loaded = harness._load_env()
        stub_env = stub.env(PERSONA, home)
        cleared = ((PROVIDER_ENV | {"STUBAI_API_KEY"}
                    | {key for key in loaded if _sensitive_env(key)}) - {"NO_PROXY"}
                   - set(stub_env))
        env = {key: "" for key in cleared}
        env.update(stub_env)
        env.update(XDG_CONFIG_HOME=str(home / "xdg-config"), XDG_DATA_HOME=str(home / "xdg-data"),
                   RUSTAIN_CONFIG_DIR=str(config), RUSTAIN_DATA_DIR=str(data),
                   RUSTAIN_LOG_PATH=str(evidence / "a.runtime.log"), COLORTERM="truecolor",
                   NO_COLOR="", NO_PROXY="127.0.0.1,localhost", TERM="xterm-256color",
                   RUSTAIN_CLIPBOARD="file")

        def safe_spawn(command, *args, **kwargs):
            actual = kwargs["env"]
            actual.pop("NO_COLOR", None)
            assert Path(command).resolve() == binary and "debug" in binary.parts
            assert kwargs["dimensions"] == (24, 80)
            assert all(actual.get(key) == "" for key in cleared)
            assert actual["RUSTAIN_DATA_DIR"] == str(data)
            return original_spawn(command, *args, **kwargs)

        monkeypatch.setattr(harness.pexpect, "spawn", safe_spawn)
        tui = _NativeTUI(binary=binary, build=False, workspace=workspace, env_overrides=env)
        tui.screens = (evidence / "a.screens.jsonl").open("w")
        raw = (evidence / "a.raw.bin").open("w", encoding="utf-8", newline="")
        try:
            tui.start()
            tui.child.delaybeforesend = 0
            tui.child.logfile_read = raw
            _wait(tui, lambda s: "Message" in s, "composer on screen")
            tui.send_message("show blocks")
            _wait(tui, _stage, "the code block on the Stage")
            tui.child.send("\x1b")
            _wait(tui, lambda s: any("[Enter] Inspect" in row for row in _rows(s)),
                  "Chat focus on the Stage (the Inspect cue)")
            tui.child.send("gg")
            screen = _wait(tui, _quiet_marked,
                           "the block header row with the marker and cue, no toast on the Stage")
            _capture(tui, evidence, "a-marked", screen)
            stage_rows = _stage_rows(screen)
            tui.child.send("\r")
            screen = _wait(tui, _opened, "the Block window with its status row")
            _capture(tui, evidence, "a-opened", screen)
            tui.child.send("$")
            screen = _wait(tui, _tail, "the tail of the 120-cell line and no continuation arrow")
            _capture(tui, evidence, "a-tail", screen)
            tui.child.send("0")
            _wait(tui, _start, "column 1 again")
            tui.child.send("G")
            screen = _wait(tui, _end, "the last row `// end-45` after G")
            _capture(tui, evidence, "a-end", screen)
            tui.child.send("c")
            clipboard = data / "clipboard.txt"
            _wait(tui, lambda s: clipboard.exists() and clipboard.stat().st_size > 0,
                  "the clipboard file written by `c`")
            copied = clipboard.read_text(encoding="utf-8")
            assert copied == _interior(), "c copies the block source, never rows or borders"
            tui.child.send("\x1b")
            screen = _wait(tui, lambda s: _back(s, stage_rows),
                           "back on the Stage with every Stage row restored")
            _capture(tui, evidence, "a-back", screen)
            # Leg B (same spawn): the capped Stage, numbers, the neighbour and `v`.
            tui.child.send("G")
            screen = _wait(tui, _capped, "the Stage bottom with `… 15 more lines` inside the box")
            _capture(tui, evidence, "b-stage-bottom", screen)
            tui.child.send("gg")
            screen = _wait(tui, _quiet_marked, "the code block focused again, no toast")
            stage_rows_b = _stage_rows(screen)
            tui.child.send("\r")
            _wait(tui, _opened, "the Block window again")
            tui.child.send("#")
            _wait(tui, lambda s: _matches_row(s, STATUS_NUMBERS), "the numbers word in the status row")
            tui.child.send("G")
            screen = _wait(tui, _numbers_end, "line 45 with its number in the gutter")
            _capture(tui, evidence, "b-numbers-end", screen)
            tui.child.send("]")
            screen = _wait(tui, _next_table, "the table neighbour (table 2/2) in Rendered")
            _capture(tui, evidence, "b-next", screen)
            tui.child.send("v")
            screen = _wait(tui, _source_table, "the table's markdown source with numbers")
            _capture(tui, evidence, "b-source", screen)
            tui.child.send("\x1b")
            screen = _wait(tui, lambda s: _back(s, stage_rows_b),
                           "back on the Stage with every Stage row restored (leg B)")
            _capture(tui, evidence, "b-back", screen)
        finally:
            try:
                tui.stop()
            finally:
                raw.close()
                tui.screens.close()
    finally:
        status = stub.stop()
    assert status == 0, f"scene provider exited {status}: {stub.rows()}"
