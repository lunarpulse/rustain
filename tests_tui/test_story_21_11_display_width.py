"""21-11 native smoke (K8): the composer on a real debug binary at 80x24.

One spawn, no API submission, isolated HOME/workspace/config: a bracketed paste
of a draft holding C (wide CJK), A (combining accent) and G (ZWJ family), longer
than one visual row, then two Backspaces and the repaint at the same fixed size.

Oracles (pyte is scalar-based and is NOT the ZWJ width oracle):
  * structural screen predicates: the second visual row continues the draft, no
    wide-glyph continuation cell is orphaned, the draft is retained after edit;
  * the caret from the program's own cursor-position sequence (pyte cursor);
  * the actual UTF-8 bytes of G and A in the raw terminal output;
  * the unit/integration keystones K2/K4 own the ZWJ cell-width arithmetic.
"""
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import tempfile
import traceback
import unicodedata

import harness
import pytest

from test_story_20_1c_locked_providers import (
    PROVIDER_ENV, _NativeTUI, _capture, _sensitive_env, _sha256, _wait,
)

C = "\u754c"
A = "e\u0301"
G = "\U0001f468\u200d\U0001f469\u200d\U0001f467"
# Inner width at 80 columns is 78 cells: 30 C (60) + A (1) + 8 C (16) = 77 cells
# fills row one (a ninth C needs 2), row two holds 2 C + G = 6 cells.
DRAFT = C * 30 + A + C * 8 + C * 2 + G
# pyte composes a base letter with its combining mark (NFC) when it draws; the
# raw terminal bytes keep the decomposed A that the program wrote.
A_SCREEN = unicodedata.normalize("NFC", A)


def _row(tui, y):
    return tui.get_screen_text().split("\n")[y]


def _orphan_continuations(tui, rows_and_limits):
    """Empty continuation cells with no wide glyph before them, for the given
    (screen row, exclusive column limit) pairs. Row two is checked only left of
    the pasted G: pyte widths ZWJ clusters per scalar and is not their oracle."""
    screen = tui._screen
    orphans = []
    for y, limit in rows_and_limits:
        for x in range(limit):
            if screen.buffer[y][x].data == "":
                prev = screen.buffer[y][x - 1].data if x else ""
                if not prev or unicodedata.east_asian_width(prev[0]) not in ("W", "F"):
                    orphans.append((x, y))
    return orphans


def _input_rows(screen_text):
    """Top border index and the composer's two text rows plus bottom border."""
    rows = screen_text.split("\n")
    top = max(i for i, row in enumerate(rows) if row.startswith("\u250c") and "Message" in row)
    return top, rows[top + 1], rows[top + 2], rows[top + 3]


def _draft_frame_ready(screen_text, caret):
    """The pasted draft's frame is whole: the 40 C, both composer rows, the closed bottom border
    and the caret from the program's own cursor-position sequence (row two, after 2 C + G = 6
    cells, +1 for the border). Rows and caret land in separate reads, so a wait on the rows alone
    can sample the caret one frame early."""
    if sum(row.count(C) for row in screen_text.split("\n")) < 40:
        return False
    try:
        top, first, second, bottom = _input_rows(screen_text)
    except ValueError:  # the composer's top border is not painted yet
        return False
    return (top == 20 and bottom.startswith("\u2514")
            and first.startswith("\u2502" + C * 30 + A_SCREEN + C * 8)
            and second.startswith("\u2502" + C * 2) and caret == (1 + 6, top + 2))


def _exercise(binary, evidence, root, monkeypatch):
    workspace = root / "fresh-cwd"
    workspace.mkdir()
    # An empty .env stops `start` from copying the project's credentials.
    (workspace / ".env").write_text("")
    home = root / "home"
    config = home / "config"
    data = home / "data"
    config.mkdir(parents=True)
    data.mkdir()
    loaded = harness._load_env()
    cleared = (PROVIDER_ENV | {"STUBAI_API_KEY"}
               | {key for key in loaded if _sensitive_env(key)}) - {"NO_PROXY"}
    env = {key: "" for key in cleared}
    runtime_log = evidence / "display-width.runtime.log"
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "xdg-config"),
               XDG_DATA_HOME=str(home / "xdg-data"), RUSTAIN_CONFIG_DIR=str(config),
               RUSTAIN_DATA_DIR=str(data), RUSTAIN_LOG_PATH=str(runtime_log),
               TERM="xterm-256color", COLORTERM="truecolor", NO_PROXY="127.0.0.1,localhost")
    original_spawn = harness.pexpect.spawn

    def safe_spawn(command, *args, **kwargs):
        actual = kwargs["env"]
        assert Path(command).resolve() == binary and "debug" in binary.parts and "release" not in binary.parts
        assert kwargs["dimensions"] == (24, 80)
        assert all(actual.get(key) == "" for key in cleared)
        assert actual["RUSTAIN_DATA_DIR"] == str(data)
        return original_spawn(command, *args, **kwargs)

    monkeypatch.setattr(harness.pexpect, "spawn", safe_spawn)
    tui = _NativeTUI(binary=binary, build=False, workspace=workspace, env_overrides=env)
    tui.screens = (evidence / "display-width.screens.jsonl").open("w")
    raw_path = evidence / "display-width.raw.bin"
    raw = raw_path.open("w", encoding="utf-8", newline="")
    try:
        tui.start()
        tui.child.delaybeforesend = 0
        tui.child.logfile_read = raw
        _wait(tui, lambda s: "Message" in s, "composer on screen")

        tui.send(f"\x1b[200~{DRAFT}\x1b[201~")
        screen = _wait(tui, lambda s: _draft_frame_ready(s, (tui._screen.cursor.x, tui._screen.cursor.y)),
                       "pasted draft painted over two visual rows with the caret after the G")
        top, first, second, bottom = _input_rows(screen)
        assert not _orphan_continuations(tui, [(top + 1, 80), (top + 2, 5)])
        _capture(tui, evidence, "display-width-pasted", screen)

        tui.send("\x7f")  # Backspace removes the whole G: the caret moves 2 cells.
        screen = _wait(tui, lambda s: (tui._screen.cursor.x, tui._screen.cursor.y) == (1 + 4, top + 2),
                       "G removed as one cluster")
        top, first, second, bottom = _input_rows(screen)
        assert first.startswith("\u2502" + C * 30 + A_SCREEN + C * 8), first
        assert second.startswith("\u2502" + C * 2), second
        assert not _orphan_continuations(tui, [(top + 1, 80), (top + 2, 5)])
        tui.send("\x7f")  # One C goes; the other stays (no orphan continuation).
        screen = _wait(tui, lambda s: (tui._screen.cursor.x, tui._screen.cursor.y) == (1 + 2, top + 2),
                       "one C removed")
        top, first, second, bottom = _input_rows(screen)
        assert first.startswith("\u2502" + C * 30 + A_SCREEN + C * 8), "the draft is retained"
        assert second.startswith("\u2502" + C), second
        assert not _orphan_continuations(tui, [(top + 1, 80), (top + 2, 4)])
        _capture(tui, evidence, "display-width-edited", screen)

        raw.flush()
        text = raw_path.read_text(encoding="utf-8")
        assert G in text, "the actual UTF-8 bytes of G reached the terminal"
        assert A in text and C in text
    finally:
        # Stop first: pexpect keeps logging the teardown output to `raw`.
        try:
            tui.stop()
        finally:
            raw.close()
            tui.screens.close()
    # The appender writes a dated file and flushes on exit: the isolated run
    # must have produced its own runtime log.
    assert list(evidence.glob("display-width.runtime.log*")), "dated runtime log missing"


@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(300)
def test_composer_wraps_and_edits_whole_graphemes_at_80x24(monkeypatch):
    parent = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())).resolve()
    parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="21-11-", dir=parent))
    manifest = {"run_id": evidence.name, "started_at": datetime.now(timezone.utc).isoformat(),
                "evidence_dir": str(evidence), "viewport": [80, 24], "result": "RUNNING"}
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    try:
        binary = harness._resolve_binary()[0].resolve()
        assert binary.is_file() and "debug" in binary.parts and "release" not in binary.parts, binary
        manifest.update(binary=str(binary), binary_sha256=_sha256(binary),
                        profile="debug", features="default")
        (evidence / "run.json").write_text(json.dumps(manifest, indent=2) + "\n")
        with tempfile.TemporaryDirectory(prefix="r21-11-") as root:
            _exercise(binary, evidence, Path(root), monkeypatch)
        assert _sha256(binary) == manifest["binary_sha256"], "binary changed during native smoke"
        manifest["result"] = "PASS"
    except BaseException:
        manifest["result"] = "FAIL"
        (evidence / "failure.txt").write_text(traceback.format_exc())
        raise
    finally:
        manifest["completed_at"] = datetime.now(timezone.utc).isoformat()
        (evidence / "run.json").write_text(json.dumps(manifest, indent=2) + "\n")
        print(f"NATIVE_21_11_EVIDENCE={evidence}", flush=True)


def test_draft_frame_predicate_rejects_a_mid_paint_caret():
    """The rows can be whole one read before the caret: the old wait (40 C painted) accepted that
    frame and the cursor assertion then failed; the frame predicate waits for the caret too."""
    rows = [""] * 24
    rows[20] = "\u250c Message " + "\u2500" * 68 + "\u2510"
    rows[21] = "\u2502" + C * 30 + A_SCREEN + C * 8
    rows[22] = "\u2502" + C * 2 + G
    rows[23] = "\u2514" + "\u2500" * 78 + "\u2518"
    screen = "\n".join(rows)
    assert sum(row.count(C) for row in screen.split("\n")) >= 40  # the old first-wait predicate
    assert not _draft_frame_ready(screen, (11, 22))
    assert not _draft_frame_ready(screen, (7, 21))
    assert _draft_frame_ready(screen, (7, 22))
    assert not _draft_frame_ready("\n".join(rows[:20] + [""] * 4), (7, 22))
