"""21-9 native smoke (slice 9-A, leg A): the folded thinking row, the Stage focus
marker and the `Thinking · ...` window on a real debug binary at 80x24.

The stored session is the committed fixture ``fixtures/sessions/story21_9_stage_polish.json``
(written by the production serializer from a conversation the production reducer
built, see conformance_21_9 ``smoke_frames``); the smoke copies it into the scratch
workspace and reopens it with ``--session``. The scene provider is only the
harness's ordinary startup provider: no message is sent and no request is made.

Leg A: the user row ``explain <T> Vec<String> <b>b</b> *x*`` paints verbatim; the
thinking part is ONE row ``│ ◇ Thinking · 4.2s · 1.3k tokens``; ``Esc`` -> Chat,
``Tab`` until that row reads ``│ ▸ Thinking · 4.2s · 1.3k tokens [Enter] Inspect``;
``Enter`` -> the ``Thinking · 4.2s · 1.3k tokens`` window with the reasoning text;
``c`` -> the clipboard file holds the original text byte for byte (its raw ESC
included); ``Esc`` -> every Stage row restored.

Offline first: RUSTAIN_STAGE_POLISH_DESKCHECK=1 with RUSTAIN_STAGE_POLISH_FRAMES=<dir>
consumes only saved Rust paints at 60x16, 80x24 and 130x30 and the torn controls
derived from them; no binary, subprocess or scene provider is started.
Predicates are anchored on whole rows; no wait is a sleep.
"""
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import shutil
import sys
import tempfile
import traceback

import harness
import pytest

sys.path.insert(0, str(Path(__file__).parent))

from fixtures.scene_provider import SceneStub  # noqa: E402
from test_story_20_1c_locked_providers import (  # noqa: E402
    PROVIDER_ENV, _NativeTUI, _capture, _sensitive_env, _sha256, _wait,
)

SCENE = Path(__file__).parent / "fixtures" / "scenes" / "block_viewer.json"
PERSONA = "scene-ci-block-viewer"
SESSION = Path(__file__).parent / "fixtures" / "sessions" / "story21_9_stage_polish.json"
SESSION_ID = "story21-9-stage-polish"
TOOLS = Path(__file__).parent / "fixtures" / "sessions" / "story21_9_tool_lines.json"
TOOLS_ID = "story21-9-tool-lines"
BRANCH = "smoke-21-9"
DESKCHECK = os.environ.get("RUSTAIN_STAGE_POLISH_DESKCHECK") == "1"

USER_ROW = r"(?:▶ )?explain <T> Vec<String> <b>b</b> \*x\*"
# The focused turn paints `▶ ` before its first row when it has no metadata row.
FOLD_ROW = r"(?:▶ )?│ ◇ Thinking · 4\.2s · 1\.3k tokens"
MARKED_ROW = r"(?:▶ )?│ ▸ Thinking · 4\.2s · 1\.3k tokens \[Enter\] Inspect"
# A toast names its severity and a message; a `✓ Read ...` tool row is not one.
TOAST = re.compile(r"(?<!✓ )(?<!✗ )\b(Warning|Info|Success|Error) [A-Z]")


def _rows(screen):
    return [row.rstrip() for row in screen.split("\n")]


def _matches_row(screen, pattern):
    compiled = re.compile(pattern)
    return any(compiled.fullmatch(row) for row in _rows(screen))


def _settled(screen):
    """Whole composer chrome: box top, a body row and the closed bottom border."""
    rows = _rows(screen)
    top = _composer_top(rows)
    return (top is not None and top >= 2 and rows[top].endswith("┐")
            and rows[top + 1].startswith("│") and rows[top + 1].endswith("│")
            and rows[-1].startswith("└") and rows[-1].endswith("┘"))


def _composer_top(rows):
    tops = [i for i, row in enumerate(rows) if row.startswith("┌ Message")]
    return tops[-1] if tops else None

def _marker_rows(screen):
    return [row for row in _rows(screen) if "▸" in row]



def _stage_rows(screen):
    """Every Stage row: all but the status row and the composer box."""
    rows = _rows(screen)
    top = _composer_top(rows)
    return rows[:top - 1] if top else rows


def _toast_rows(screen):
    return [row for row in _stage_rows(screen) if TOAST.search(row)]


def _thinking_fold(screen):
    return _settled(screen) and _matches_row(screen, FOLD_ROW)


def _verbatim_user(screen):
    return _settled(screen) and _matches_row(screen, USER_ROW)


def _marked(screen):
    return (_settled(screen) and _matches_row(screen, MARKED_ROW)
            and sum(1 for row in _rows(screen) if "▸" in row) == 1)


def _quiet_marked(screen):
    return _marked(screen) and not _toast_rows(screen)


def _title_row(screen):
    return any(row.lstrip().startswith("┌Thinking · 4.2s · 1.3k tokens") for row in _rows(screen))


def _opened(screen):
    rows = _rows(screen)
    return (_title_row(screen)
            and any("Compare parser A and parser B." in row for row in rows)
            and any(row.lstrip("│ ").startswith("thinking · ") and "Rendered" in row for row in rows)
            and any("[Esc] Back" in row for row in rows)
            and not any("[#]" in row or "[w]" in row for row in rows))


def _back(screen, stage_rows=None):
    return (_marked(screen) and not _title_row(screen)
            and (stage_rows is None or _stage_rows(screen) == stage_rows))


def _reasoning_text():
    """The reasoning text exactly as the stored session carries it."""
    found = []

    def walk(value):
        if isinstance(value, dict):
            if value.get("kind") == "reasoning" and "text" in value:
                found.append(value["text"])
            for inner in value.values():
                walk(inner)
        elif isinstance(value, list):
            for inner in value:
                walk(inner)

    walk(json.loads(SESSION.read_text()))
    assert len(found) == 1, found
    return found[0]

# Leg B (slice 9-B): the humanized tool rows and the launchpad. The one focused
# row (after Esc) leads with `▸ ` and may end with the Inspect cue.
_LEAD = r"(?:▶ )?│ (?:┄|▸) "
_CUE = r"(?: \[Enter\] Inspect)?"
TOOL_ROWS = (
    _LEAD + r"✓ Read src/x\.rs:1-80 · 0\.3s" + _CUE,
    _LEAD + r"✓ Edit src/parser\.rs \+12 −3 · 0\.1s" + _CUE,
    _LEAD + r"✓ Write src/new\.rs \+40 · 0\.1s" + _CUE,
    _LEAD + r"✓ Bash cargo test --lib · 2\.0s" + _CUE,
)
AGE = r"(?:just now|\d+[mh] ago|[A-Z][a-z]{2} \d+|\d{4}-\d\d-\d\d)"


def _tool_rows(screen):
    return (_settled(screen) and all(_matches_row(screen, row) for row in TOOL_ROWS)
            and not any('{"' in row or "→" in row for row in _stage_rows(screen)))


def _launchpad_rows(screen):
    """The rows from the heading through the key line, or None when torn."""
    rows = _rows(screen)
    heads = [i for i, row in enumerate(rows) if row.strip() == "Welcome to Rustain."]
    keys = [i for i, row in enumerate(rows) if re.fullmatch(r"\s*\^X \? Help · .*", row)]
    if len(heads) != 1 or len(keys) != 1 or keys[0] <= heads[0]:
        return None
    return rows[heads[0]:keys[0] + 1]


def _launchpad(screen):
    block = _launchpad_rows(screen)
    if not _settled(screen) or block is None or len(block) < 11:
        return False
    rows = _rows(screen)
    if "│ Normal │" not in rows[_composer_top(rows) - 1]:
        return False  # the status row is whole and sits right above the composer
    return (re.fullmatch(rf"\s*\S+ · {BRANCH}", block[1]) is not None
            and " · " in block[2] and block[3].strip() == ""
            and block[4].strip() == "Type a message to start."
            and any(row.strip() == "Recent" for row in block)
            and any(re.fullmatch(rf"\s+Tool lines smoke · {AGE}", row) for row in block)
            and not any("Ready" in row or "│" in row for row in block)
            and not _toast_rows(screen))

def _frame(frames, size, name):
    return json.loads((frames / f"{size}-{name}.json").read_text())["screen"]


def _deskcheck_b(frames):
    for size in ("60x16", "80x24", "130x30"):
        tools, pad = _frame(frames, size, "tools"), _frame(frames, size, "launchpad")
        assert _tool_rows(tools) and not _tool_rows(tools.replace("0.3s", "0.3")), size
        assert _launchpad(pad), size
        # Torn controls: a cut row, a missing status row, rows shifted by one, a toast.
        assert not _tool_rows(tools.replace("+12 −3", "+12 −")), size
        assert not _launchpad(pad.replace(BRANCH, BRANCH[:-1])), size
        rows = _rows(pad)
        top = _composer_top(rows)
        assert not _launchpad("\n".join(rows[:top - 1] + rows[top:])), size
        head = next(i for i, row in enumerate(rows) if row.strip() == "Welcome to Rustain.")
        assert not _launchpad("\n".join(rows[:head + 1] + [""] + rows[head + 1:-1] + rows[-1:])), size
        toasted = rows[:]
        toasted[head + 1] = toasted[head + 1][:4] + "Warning Model discovery for 'anthropic' returned 0 models."
        assert not _launchpad("\n".join(toasted)), size
        assert not _launchpad(pad.replace("Welcome to Rustain.", "Ready to start.")), size



def _deskcheck():
    frames = Path(os.environ["RUSTAIN_STAGE_POLISH_FRAMES"])
    for size in ("60x16", "80x24", "130x30"):
        stage, marked, opened, back = (
            _frame(frames, size, name) for name in ("stage", "marked", "opened", "back"))
        # The predicates accept the real paints and reject the others.
        assert _thinking_fold(stage) and not _marked(stage), size
        assert _quiet_marked(marked) and not _opened(marked), size
        assert _opened(opened) and not _marked(opened) and not _thinking_fold(opened), size
        assert _back(back, _stage_rows(marked)) and not _opened(back), size
        assert _verbatim_user(stage), size
        # Torn frames: a cut row, a missing composer, a half-painted title or rail.
        assert not _marked(marked.replace("Inspect", "Insp")), size
        assert not _thinking_fold(stage.replace("1.3k tokens", "1.3k tok")), size
        assert not _quiet_marked("\n".join(_rows(marked)[:-1])), size
        assert not _opened(opened.replace("[Esc] Back", "[Esc] Ba")), size
        assert not _opened(opened.replace("┌Thinking · 4.2s", "┌Thinking · 4.")), size
        assert not _opened(opened.replace("Compare parser A and parser B.", "Compare par")), size
        assert not _back(back, _stage_rows(marked)[:-1] + ["torn"]), size
        # A toast over a Stage row makes the baseline unstable: marked, not quiet.
        rows = marked.split("\n")
        last = len(_stage_rows(marked)) - 1
        rows[last] = rows[last][:10] + "Warning Model discovery for 'anthropic' returned 0 models."
        toasted = "\n".join(rows)
        assert _toast_rows(toasted) and not _quiet_marked(toasted), size
        assert _quiet_marked(marked), size
    _deskcheck_b(frames)
    # 80x24 is the live size: the user row is whole and verbatim there.
    assert _verbatim_user(_frame(frames, "80x24", "stage"))
    assert not _verbatim_user(_frame(frames, "80x24", "stage").replace("<b>b</b>", "b"))
    # The thinking row stays one row of its own at every size.
    for size in ("60x16", "80x24", "130x30"):
        fold = [row for row in _rows(_frame(frames, size, "stage")) if "◇ Thinking" in row]
        assert len(fold) == 1 and fold[0].startswith("│ ◇ Thinking"), (size, fold)


@pytest.fixture(scope="session", autouse=True)
def build_binary():
    """Offline desk-check starts no binary; live runs use the default resolution."""
    if not DESKCHECK:
        assert harness._resolve_binary()[0].is_file()


@pytest.mark.skipif(not DESKCHECK, reason="offline desk-check only")
def test_stage_polish_predicates_accept_real_saved_frames_and_reject_torn_ones():
    _deskcheck()


@pytest.mark.skipif(DESKCHECK, reason="live run is skipped during the offline desk-check")
@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(240)
def test_thinking_row_marker_window_and_copy_at_80x24(monkeypatch):
    parent = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())).resolve()
    parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="21-9-a-", dir=parent))
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
        with tempfile.TemporaryDirectory(prefix="r21-9-") as root_name:
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
        print(f"NATIVE_21_9_EVIDENCE={evidence}", flush=True)


def _spawn(binary, evidence, root, original_spawn, monkeypatch):
    workspace = root / "workspace"
    sessions = workspace / ".claude" / "sessions"
    sessions.mkdir(parents=True)
    # Flat store layout: `{id}.meta.json` holds the whole PersistedConversation.
    shutil.copy(SESSION, sessions / f"{SESSION_ID}.meta.json")
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
        tui = _NativeTUI(binary=binary, build=False, workspace=workspace, fresh=False,
                         env_overrides=env, extra_args=["--session", SESSION_ID])
        tui.screens = (evidence / "a.screens.jsonl").open("w")
        raw = (evidence / "a.raw.bin").open("w", encoding="utf-8", newline="")
        try:
            tui.start()
            tui.child.delaybeforesend = 0
            tui.child.logfile_read = raw
            screen = _wait(tui, lambda s: _thinking_fold(s) and _verbatim_user(s),
                           "the stored session: the verbatim user row and the folded thinking row")
            _capture(tui, evidence, "a-stage", screen)
            tui.child.send("\x1b")
            screen = _wait(tui, lambda s: _settled(s) and len(_marker_rows(s)) == 1,
                           "Chat focus: exactly one marked Stage item")
            for _ in range(4):
                if _marked(screen):
                    break
                tui.child.send("\t")
                screen = _wait(tui, lambda s, prev=_marker_rows(screen): (
                    _settled(s) and len(_marker_rows(s)) == 1 and _marker_rows(s) != prev),
                    "the marker on the next Stage item")
            else:
                raise AssertionError("Tab never reached the thinking row:\n" + screen)
            # Expiring startup toasts leave on their own: wait, never dismiss with a key.
            screen = _wait(tui, _quiet_marked, "the thinking row marked with no toast on the Stage")
            _capture(tui, evidence, "a-marked", screen)
            stage_rows = _stage_rows(screen)
            tui.child.send("\r")
            screen = _wait(tui, _opened, "the Thinking window with its text and Back rail")
            _capture(tui, evidence, "a-opened", screen)
            tui.child.send("c")
            clipboard = data / "clipboard.txt"
            _wait(tui, lambda s: clipboard.exists() and clipboard.stat().st_size > 0,
                  "the clipboard file written by `c`")
            copied = clipboard.read_bytes().decode("utf-8")
            assert copied == _reasoning_text(), "c copies the original text, raw ESC included"
            assert "\x1b" in copied, "the control byte the window never shows is kept"
            tui.child.send("\x1b")
            screen = _wait(tui, lambda s: _back(s, stage_rows),
                           "back on the Stage with every Stage row restored")
            _capture(tui, evidence, "a-back", screen)
        finally:
            try:
                tui.stop()
            finally:
                raw.close()
                tui.screens.close()
    finally:
        status = stub.stop()
    assert status == 0, f"scene provider exited {status}: {stub.rows()}"


@pytest.mark.skipif(DESKCHECK, reason="live run is skipped during the offline desk-check")
@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(240)
def test_tool_rows_and_launchpad_at_80x24(monkeypatch):
    """Leg B (slice 9-B): the humanized tool rows of the stored session, then the
    launchpad of a fresh conversation in the same git workspace."""
    parent = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())).resolve()
    parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="21-9-b-", dir=parent))
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
        with tempfile.TemporaryDirectory(prefix="r21-9b-") as root_name:
            _spawn_b(binary, evidence, Path(root_name), original_spawn, monkeypatch)
        assert _sha256(binary) == manifest["binary_sha256"], "binary changed during native smoke"
        manifest["result"] = "PASS"
    except BaseException:
        manifest["result"] = "FAIL"
        (evidence / "failure.txt").write_text(traceback.format_exc())
        raise
    finally:
        manifest["completed_at"] = datetime.now(timezone.utc).isoformat()
        (evidence / "run.json").write_text(json.dumps(manifest, indent=2) + "\n")
        print(f"NATIVE_21_9_EVIDENCE={evidence}", flush=True)


def _spawn_b(binary, evidence, root, original_spawn, monkeypatch):
    workspace = root / "workspace"
    sessions = workspace / ".claude" / "sessions"
    sessions.mkdir(parents=True)
    shutil.copy(TOOLS, sessions / f"{TOOLS_ID}.meta.json")
    (workspace / ".env").write_text("")
    (workspace / ".git").mkdir()
    (workspace / ".git" / "HEAD").write_text(f"ref: refs/heads/{BRANCH}\n")
    home = root / "home"
    config, data = home / "config", home / "data"
    config.mkdir(parents=True)
    data.mkdir()
    stub = SceneStub(SCENE, evidence / "b.requests.jsonl").start()
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
                   RUSTAIN_LOG_PATH=str(evidence / "b.runtime.log"), COLORTERM="truecolor",
                   NO_COLOR="", NO_PROXY="127.0.0.1,localhost", TERM="xterm-256color",
                   RUSTAIN_CLIPBOARD="file")

        def safe_spawn(command, *args, **kwargs):
            actual = kwargs["env"]
            actual.pop("NO_COLOR", None)
            assert Path(command).resolve() == binary and "debug" in binary.parts
            assert kwargs["dimensions"] == (24, 80)
            assert actual["RUSTAIN_DATA_DIR"] == str(data)
            return original_spawn(command, *args, **kwargs)

        monkeypatch.setattr(harness.pexpect, "spawn", safe_spawn)

        def run(leg, fresh, args, steps):
            tui = _NativeTUI(binary=binary, build=False, workspace=workspace, fresh=fresh,
                             env_overrides=env, extra_args=args)
            tui.screens = (evidence / f"b-{leg}.screens.jsonl").open("w")
            raw = (evidence / f"b-{leg}.raw.bin").open("w", encoding="utf-8", newline="")
            try:
                tui.start()
                tui.child.delaybeforesend = 0
                tui.child.logfile_read = raw
                steps(tui)
            finally:
                try:
                    tui.stop()
                finally:
                    raw.close()
                    tui.screens.close()

        def tools(tui):
            _wait(tui, lambda s: _settled(s) and _matches_row(s, USER_ROW_B),
                  "the stored session's user row")
            tui.child.send("\x1b")
            _wait(tui, lambda s: _settled(s) and len(_marker_rows(s)) >= 1, "Chat focus")
            tui.child.send("zR")
            screen = _wait(tui, lambda s: _tool_rows(s) and not _toast_rows(s),
                           "the four humanized tool rows on the unfolded Stage")
            _capture(tui, evidence, "b-tools", screen)

        def launchpad(tui):
            screen = _wait(tui, _launchpad,
                           "the launchpad: workspace and branch, Type a message, Recent, keys")
            _capture(tui, evidence, "b-launchpad", screen)

        run("tools", False, ["--session", TOOLS_ID], tools)
        run("launchpad", True, [], launchpad)
    finally:
        status = stub.stop()
    assert status == 0, f"scene provider exited {status}: {stub.rows()}"


USER_ROW_B = r"(?:▶ )?show the tool rows"
