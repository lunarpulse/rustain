"""21-12 native smoke: markdown structure, a literal user message and Stage
links on a real debug binary at 80x24 (scene provider, two spawns).

(a) `TERM=xterm-256color TERM_PROGRAM=herdr TMUX= STY=`: OSC 8 on, so the label
    `claude.ai` (never its host suffix) and the autolink arrive as per-glyph envelopes.
(b) `TERM=xterm-256color STY=21-12` (GNU screen): zero OSC 8 bytes, same rows.

Predicates are anchored on whole rows, never on substrings.
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

SCENE = Path(__file__).parent / "fixtures" / "scenes" / "markdown_structure.json"
PERSONA = "scene-ci-markdown"
LINK_URL = "https://evil.test/x"
AUTO_URL = "https://example.com/a"
LABEL = "claude.ai"
OSC8 = re.compile(r"\x1b\]8;[^\x07]*\x07")
TYPED = "literal **x** <T>"


def _rows(screen):
    return [row.rstrip() for row in screen.split("\n")]


def _has_row(screen, expected):
    return expected in _rows(screen)


def _matches_row(screen, pattern):
    compiled = re.compile(pattern)
    return any(compiled.fullmatch(row) for row in _rows(screen))


def _ready(screen):
    """`Ready` on the status bar: the row directly above the composer's top border."""
    rows = screen.split("\n")
    tops = [y for y, row in enumerate(rows) if row.startswith("┌")]
    return bool(tops) and tops[-1] > 0 and status_ready(rows[tops[-1] - 1])


def _structure_one(screen):
    rows = _rows(screen)
    return (
        _ready(screen)
        and "│ Report" in rows
        and rows.index("│ Report") + 1 < len(rows)
        and re.fullmatch(r"│ ─{30,}", rows[rows.index("│ Report") + 1]) is not None
        and "│ • level one" in rows
        and "│   • level two" in rows
        and "│     • level three" in rows
        and "│       • level four" in rows
        and "│ Vec<String> and SELECT * FROM t and 2 * 3" in rows
    )


def _structure_two(screen):
    return (
        _ready(screen)
        and _matches_row(screen, r"│ ┌─+┬─+┐")
        and _matches_row(screen, r"│ │ name +│ qty │")
        and _matches_row(screen, r"│ │ apple │ +3 │")
        and _matches_row(screen, r"│ └─+┴─+┘")
        and _has_row(screen, "│ │ outer")
        and _has_row(screen, "│ │ │ inner")
    )


def _links(screen):
    return (
        _ready(screen)
        and _has_row(screen, f"│ {LABEL} (evil.test)")
        and _has_row(screen, f"│ {AUTO_URL}")
        and _has_row(screen, "│ unsafe (javascript:alert(1))")
    )


def _typed(screen):
    return _ready(screen) and _has_row(screen, TYPED) and _has_row(screen, "│ noted")


def _label_cells(tui, screen):
    """Pyte cells of the link label row: `(label cells, suffix cells)`."""
    rows = _rows(screen)
    y = rows.index(f"│ {LABEL} (evil.test)")
    label = [tui._screen.buffer[y][x] for x in range(2, 2 + len(LABEL))]
    suffix = [tui._screen.buffer[y][x]
              for x in range(2 + len(LABEL), 2 + len(LABEL) + len(" (evil.test)"))]
    return label, suffix


def _envelopes(raw_text, url):
    pattern = re.compile(re.escape("\x1b]8;;" + url + "\x07") + r"([^\x00-\x1f\x7f])"
                         + re.escape("\x1b]8;;\x07"))
    return pattern, list(pattern.finditer(raw_text))


def _spawn_case(name, binary, evidence, root, original_spawn, monkeypatch, expected):
    """One spawn: returns the OSC 8 byte count of the link reply's paint."""
    workspace = root / "fresh-cwd"
    workspace.mkdir()
    # An empty .env stops `start` from copying the project's credentials.
    (workspace / ".env").write_text("")
    home = root / "home"
    config = home / "config"
    data = home / "data"
    config.mkdir(parents=True)
    data.mkdir()
    stub = SceneStub(SCENE, evidence / f"{name}.requests.jsonl").start()
    try:
        loaded = harness._load_env()
        stub_env = stub.env(PERSONA, home)
        cleared = ((PROVIDER_ENV | {"STUBAI_API_KEY"}
                    | {key for key in loaded if _sensitive_env(key)}) - {"NO_PROXY"}
                   - set(stub_env))
        env = {key: "" for key in cleared}
        runtime_log = evidence / f"{name}.runtime.log"
        env.update(stub_env)
        env.update(XDG_CONFIG_HOME=str(home / "xdg-config"), XDG_DATA_HOME=str(home / "xdg-data"),
                   RUSTAIN_CONFIG_DIR=str(config), RUSTAIN_DATA_DIR=str(data),
                   RUSTAIN_LOG_PATH=str(runtime_log), COLORTERM="truecolor", NO_COLOR="",
                   NO_PROXY="127.0.0.1,localhost", **expected)

        def safe_spawn(command, *args, **kwargs):
            actual = kwargs["env"]
            actual.pop("NO_COLOR", None)
            assert Path(command).resolve() == binary and "debug" in binary.parts and "release" not in binary.parts
            assert kwargs["dimensions"] == (24, 80)
            assert all(actual.get(key) == "" for key in cleared)
            assert actual["RUSTAIN_DATA_DIR"] == str(data)
            assert actual["ANTHROPIC_BASE_URL"].startswith("http://127.0.0.1:")
            assert {key: actual[key] for key in expected} == expected
            return original_spawn(command, *args, **kwargs)

        monkeypatch.setattr(harness.pexpect, "spawn", safe_spawn)
        tui = _NativeTUI(binary=binary, build=False, workspace=workspace, env_overrides=env)
        tui.screens = (evidence / f"{name}.screens.jsonl").open("w")
        raw_path = evidence / f"{name}.raw.bin"
        raw = raw_path.open("w", encoding="utf-8", newline="")
        osc8_bytes = None
        try:
            tui.start()
            tui.child.delaybeforesend = 0
            tui.child.logfile_read = raw
            _wait(tui, lambda s: "Message" in s, "composer on screen")

            tui.send_message("show structure one")
            screen = _wait(tui, _structure_one, "heading, 4-level list and literal prose")
            _capture(tui, evidence, f"{name}-structure-one", screen)

            tui.send_message("show structure two")
            screen = _wait(tui, _structure_two, "aligned table box and nested quote rails")
            _capture(tui, evidence, f"{name}-structure-two", screen)

            raw.flush()
            before = len(raw_path.read_bytes())
            tui.send_message("show links")
            screen = _wait(tui, _links, "link rows with the host suffix")
            _capture(tui, evidence, f"{name}-links", screen)
            raw.flush()
            paint = raw_path.read_bytes()[before:].decode("utf-8", errors="replace")
            osc8_bytes = sum(len(match.group(0).encode()) for match in OSC8.finditer(paint))
            label_cells, suffix_cells = _label_cells(tui, screen)
            assert all(cell.underscore for cell in label_cells), label_cells
            assert not any(cell.underscore for cell in suffix_cells), suffix_cells
            if name == "a-herdr":
                pattern, found = _envelopes(paint, LINK_URL)
                glyphs = "".join(match.group(1) for match in found)
                assert len(found) == paint.count("\x1b]8;;" + LINK_URL + "\x07") > 0, paint
                assert paint.count("\x1b]8;;\x07") == paint.count("\x1b]8;;" + LINK_URL + "\x07") \
                    + paint.count("\x1b]8;;" + AUTO_URL + "\x07")
                assert glyphs.replace(LABEL, "") == "", ("only the label is linked", glyphs)
                assert LABEL in glyphs
                auto_pattern, auto = _envelopes(paint, AUTO_URL)
                assert AUTO_URL in "".join(match.group(1) for match in auto)
                assert "javascript" not in "".join(OSC8.findall(paint))
                assert osc8_bytes > 0
            else:
                assert raw_path.read_bytes().count(b"\x1b]8;") == 0, "GNU screen gets no OSC 8 bytes"
                assert osc8_bytes == 0

            tui.send_message(TYPED)
            screen = _wait(tui, _typed, "the typed user message exactly as typed")
            _capture(tui, evidence, f"{name}-typed", screen)
            raw.flush()
        finally:
            # Stop first: pexpect keeps logging the teardown output to `raw`.
            try:
                tui.stop()
            finally:
                raw.close()
                tui.screens.close()
    finally:
        status = stub.stop()
    assert status == 0, f"scene provider exited {status}: {stub.rows()}"
    logs = sorted(evidence.glob(f"{name}.runtime.log*"))
    assert any(re.fullmatch(rf"{re.escape(name)}\.runtime\.log\.\d{{4}}-\d{{2}}-\d{{2}}", log.name)
               and log.is_file() for log in logs), "dated runtime log missing"
    return osc8_bytes


@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(600)
def test_markdown_structure_and_stage_links_at_80x24(monkeypatch):
    parent = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())).resolve()
    parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="21-12-", dir=parent))
    manifest = {"run_id": evidence.name, "started_at": datetime.now(timezone.utc).isoformat(),
                "evidence_dir": str(evidence), "viewport": [80, 24],
                "spawns": ["a-herdr", "b-screen"], "result": "RUNNING"}
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    original_spawn = harness.pexpect.spawn
    try:
        binary = harness._resolve_binary()[0].resolve()
        assert binary.is_file() and "debug" in binary.parts and "release" not in binary.parts, binary
        manifest.update(binary=str(binary), binary_sha256=_sha256(binary),
                        profile="debug", features="default")
        (evidence / "run.json").write_text(json.dumps(manifest, indent=2) + "\n")
        cases = [
            ("a-herdr", {"TERM": "xterm-256color", "TERM_PROGRAM": "herdr", "TMUX": "", "STY": ""}),
            ("b-screen", {"TERM": "xterm-256color", "TERM_PROGRAM": "", "TMUX": "", "STY": "21-12"}),
        ]
        for name, expected in cases:
            with tempfile.TemporaryDirectory(prefix=f"r21-12-{name}-") as root:
                manifest[f"{name}_osc8_bytes_link_reply"] = _spawn_case(
                    name, binary, evidence, Path(root), original_spawn, monkeypatch, expected)
        assert manifest["a-herdr_osc8_bytes_link_reply"] > 0
        assert manifest["b-screen_osc8_bytes_link_reply"] == 0
        assert _sha256(binary) == manifest["binary_sha256"], "binary changed during native smoke"
        # No credential value from the developer environment reached any artifact.
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
        print(f"NATIVE_21_12_EVIDENCE={evidence}", flush=True)
