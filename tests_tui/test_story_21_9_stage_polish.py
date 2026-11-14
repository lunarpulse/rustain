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


@pytest.fixture(scope="session", autouse=True)
def build_binary():
    """Assert the supplied or resolved binary exists."""
    assert harness._resolve_binary()[0].is_file()


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
