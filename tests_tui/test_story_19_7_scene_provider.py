"""Story 19.7 — the deterministic scene provider, driven through the real binary.

This is the first test in the non-API lane that sends a **model turn**: the binary
talks to `fixtures/scene_provider.py` over HTTP exactly as it would talk to
Anthropic, so the turn needs no provider account and replays byte-stable.

Every assertion here is product-side — the request log (what the binary *sent*), the
usage ledger (millisecond and tokens the binary *minted*), the session file (what the
binary *wrote*) and the permission prompt (what the product *asked*). The one place a
served string is asserted is the rendering positive control in
`test_text_turn_completes_through_the_env_front_door`, and it is paired with all four.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).parent))

from fixtures.scene_provider import SceneStub
from harness import PROJECT_ROOT, RustainTUI

SCENE = Path(__file__).parent / "fixtures" / "scenes" / "smoke.json"
RELEASE_BINARY = PROJECT_ROOT / "target" / "release" / "rustain"
SENTINEL = "SENTINEL-LEAK-19-7"

pytestmark = pytest.mark.story_19_7


# ── Helpers ──────────────────────────────────────────────────────────────────


def _wait_for(predicate, timeout: float = 15.0, interval: float = 0.25):
    """Poll until `predicate()` is truthy; return its value (falsy on timeout)."""
    deadline = time.monotonic() + timeout
    value = predicate()
    while not value and time.monotonic() < deadline:
        time.sleep(interval)
        value = predicate()
    return value


def _ledger_rows(tui: RustainTUI) -> list[dict]:
    """Usage-ledger rows from THIS run's workspace (never `$HOME`)."""
    usage = tui.wp / ".rustain_data" / "usage"
    if not usage.exists():
        return []
    return [
        json.loads(line)
        for path in sorted(usage.glob("*.jsonl"))
        for line in path.read_text().splitlines()
        if line.strip()
    ]


def _turn_rows(stub: SceneStub) -> list[dict]:
    """Request-log rows for scene turns (boot calls carry a `kind` and no `turn`)."""
    return [row for row in stub.rows() if "turn" in row]


def _session_files(tui: RustainTUI) -> list[Path]:
    return sorted((tui.wp / ".claude" / "sessions").glob("*.meta.json"))


def _assistant_texts(tui: RustainTUI) -> list[str]:
    """Non-empty assistant content the binary actually persisted."""
    texts: list[str] = []
    for path in _session_files(tui):
        doc = json.loads(path.read_text())
        for message in doc.get("messages", []):
            if message.get("role") == "assistant" and str(message.get("content", "")).strip():
                texts.append(message["content"])
    return texts


# ── Fixtures ─────────────────────────────────────────────────────────────────


@pytest.fixture
def stub(tmp_path):
    """A scene provider on an ephemeral port, whose exit status is an assertion.

    Default expectation is a clean exit (0). A test that provokes a desync or an
    unknown path sets `stub.expect_status = 1` — Python's default SIGTERM death
    would be -15 on every run, so this is falsifiable in both directions.
    """
    instance = SceneStub(SCENE, tmp_path / "requests.jsonl").start()
    instance.expect_status = 0
    try:
        yield instance
    finally:
        status = instance.stop()
        assert status == instance.expect_status, (
            f"scene provider exited {status}, expected {instance.expect_status}; "
            f"log rows: {instance.rows()}"
        )


@pytest.fixture
def scratch_home(tmp_path) -> Path:
    home = tmp_path / "home"
    home.mkdir()
    return home


# ── AC1 — a text turn and a tool turn through the production front door ───────


def test_text_turn_completes_through_the_env_front_door(stub, scratch_home):
    """AC1(b): the env provider path — J0's front door — carries a whole turn."""
    with RustainTUI(fresh=True, build=False, env_overrides=stub.env("scene-ci", scratch_home)) as tui:
        tui.send_message("scene ping")

        # Rendering positive control (the one served string this file asserts).
        assert tui.wait_for_screen("scene pong", timeout=30), tui.get_screen_text()
        # `wait_for_screen`, not `wait_for_idle`: the latter falls back to a sleep
        # and therefore cannot fail.
        assert tui.wait_for_screen("Ready", timeout=30) is True, tui.get_screen_text()
        tui.assert_screen_not_contains("Stream disconnected")
        # Guard first: an empty/missing log would make the next assertion vacuous.
        assert tui.log_lines(), "the isolated log is empty — nothing to assert against"
        tui.assert_log_not_contains("synthesizing end")

        # Product-minted evidence: 13-digit millisecond AND a non-zero output count.
        # The failure path writes a row too, with tokensOut 0 — so the millisecond
        # alone proves only staleness, never success.
        rows = _wait_for(lambda: _ledger_rows(tui))
        assert len(rows) == 1, rows
        # AC/A8 pin `"timestampMs":[0-9]{13}` — assert the numeric range, not
        # string length (a negative 12-digit value would pass a length check).
        timestamp = rows[0]["timestampMs"]
        assert isinstance(timestamp, int) and 10**12 <= timestamp < 10**13, rows[0]
        assert rows[0]["usage"]["tokensOut"] >= 1, rows[0]

        # What the binary sent.
        turns = _turn_rows(stub)
        assert len(turns) == 1, stub.rows()
        assert turns[0]["stream"] is True
        assert "scene ping" in turns[0]["last_user"]
        assert turns[0]["auth"] == "x-api-key"
        assert turns[0]["persona"] == "scene-ci"
        assert turns[0]["desync"] is False

        # The boot calls the stub must answer or the receipt fills with WARNs.
        kinds = {row.get("kind") for row in stub.rows()}
        assert {"health_check", "probe", "models_dev"} <= kinds, stub.rows()


def test_tool_turn_prompts_then_writes_the_scripted_file(stub, scratch_home):
    """AC1(c): a scripted tool_use reaches the real permission gate and the real tool."""
    # The harness default allow-list pre-approves Write, which would skip the prompt
    # entirely; `approve_permission()` is a blind `y`, so the prompt must be waited for.
    with RustainTUI(
        fresh=True,
        build=False,
        allowed_tools=["Read"],
        env_overrides=stub.env("scene-tools", scratch_home),
    ) as tui:
        tui.send_message("write the scene file")

        assert tui.wait_for_screen("[y] Allow", timeout=30), tui.get_screen_text()
        tui.approve_permission()

        written = tui.wp / "scene.txt"
        assert _wait_for(lambda: written.exists()), tui.get_screen_text()
        assert written.read_text() == "scene bytes from the smoke scene\n"

        # The follow-up request is the binary's own work: it carries the tool_result.
        turns = _wait_for(lambda: _turn_rows(stub) if len(_turn_rows(stub)) >= 2 else None)
        assert turns and len(turns) == 2, stub.rows()
        assert turns[0]["tool_results"] == 0
        assert turns[1]["turn"] == 2
        assert turns[1]["tool_results"] == 1
        assert turns[1]["desync"] is False
        assert tui.wait_for_screen("Ready", timeout=30) is True, tui.get_screen_text()


# ── AC2 — loud desync, loud unknown path, no credential in a receipt ──────────


def test_desync_is_loud_product_side(stub, scratch_home):
    """AC2(a): unscripted text is a finding, never auto-advanced past."""
    stub.expect_status = 1
    with RustainTUI(fresh=True, build=False, env_overrides=stub.env("scene-ci", scratch_home)) as tui:
        tui.send_message("a sentence the scene never scripted")

        turns = _wait_for(lambda: _turn_rows(stub))
        assert len(turns) == 1, stub.rows()
        assert turns[0]["desync"] is True
        assert turns[0]["turn"] == 1

        rows = _wait_for(lambda: _ledger_rows(tui))
        assert len(rows) == 1, rows
        assert rows[0]["usage"]["tokensOut"] == 0, rows[0]

        # The screen and the persisted assistant row both carry the stub's own
        # error.message rendered verbatim (`stream.rs` folds StreamChunk::Error into
        # the assistant text) — the echo class, so that text is never asserted. What
        # IS asserted product-side: the scene's scripted answer was never recorded.
        assert _wait_for(lambda: _session_files(tui)), "no session file was written"
        session_text = "\n".join(path.read_text() for path in _session_files(tui))
        assert "scene pong" not in session_text
        assert not any("scene pong" in text for text in _assistant_texts(tui))

        # Never auto-advanced past: the cursor did not move, so turn 1 is still the
        # one being expected. (Delete the expect check and these become turns 2..n.)
        # The second user turn also triggers the product's one-shot title call
        # (`title_trigger = conversation.turns.len() == 2`, tools: 0, max_tokens: 30),
        # which is a real provider request and desyncs here too — hence "every row",
        # not "exactly two rows".
        tui.send_message("still not the scripted sentence")
        turns = _wait_for(lambda: _turn_rows(stub) if len(_turn_rows(stub)) >= 2 else None)
        assert turns and len(turns) >= 2, stub.rows()
        assert [row["turn"] for row in turns] == [1] * len(turns), turns
        assert all(row["desync"] is True for row in turns), turns


def test_unknown_path_is_loud(stub):
    """AC2(b): anything outside the three answered endpoints is 404 + exit 1."""
    stub.expect_status = 1
    with pytest.raises(urllib.error.HTTPError) as excinfo:
        urllib.request.urlopen(f"{stub.url}/not-an-anthropic-endpoint", timeout=5)
    assert excinfo.value.code == 404

    rows = [row for row in stub.rows() if row.get("kind") == "unknown"]
    assert len(rows) == 1, stub.rows()
    assert rows[0]["path"] == "/not-an-anthropic-endpoint"

    # A verb outside GET/POST is equally loud — the stdlib default would be a
    # silent 501 that logs nothing and marks nothing.
    put = urllib.request.Request(
        f"{stub.url}/v1/messages", data=b"{}", method="PUT"
    )
    with pytest.raises(urllib.error.HTTPError) as put_exc:
        urllib.request.urlopen(put, timeout=5)
    assert put_exc.value.code == 404

    # A stream-less POST that is not the pinned health-check shape
    # (no `stream` AND `max_tokens: 1`) is not swallowed as a clean health check.
    turnless = urllib.request.Request(
        f"{stub.url}/v1/messages",
        data=json.dumps({"model": "x", "max_tokens": 5, "messages": []}).encode(),
        headers={"content-type": "application/json"},
        method="POST",
    )
    with pytest.raises(urllib.error.HTTPError) as turnless_exc:
        urllib.request.urlopen(turnless, timeout=5)
    assert turnless_exc.value.code == 404

    # An unparseable body never becomes a health check either.
    garbage = urllib.request.Request(
        f"{stub.url}/v1/messages", data=b"{not json", method="POST"
    )
    with pytest.raises(urllib.error.HTTPError) as garbage_exc:
        urllib.request.urlopen(garbage, timeout=5)
    assert garbage_exc.value.code == 400

    rows = [row for row in stub.rows() if row.get("kind") == "unknown"]
    assert [(row["method"], row["path"]) for row in rows] == [
        ("GET", "/not-an-anthropic-endpoint"),
        ("PUT", "/v1/messages"),
        ("POST", "/v1/messages"),  # stream-less, not the pinned health-check shape
        ("POST", "/v1/messages"),  # unparseable body
    ], stub.rows()


def test_no_credential_can_reach_the_request_log(stub, scratch_home, tmp_path):
    """AC2(c): the leak is real, the scheme is logged, the value never is."""
    # Half 1 — the leak, unmitigated: `.env` exports a bearer token and the binary
    # prefers AUTH_TOKEN over API_KEY, so a driver that sets only the api key sends
    # someone's real credential to the provider it is talking to.
    stub.expect_status = 1
    leaky = stub.env("scene-ci", scratch_home)
    leaky["ANTHROPIC_AUTH_TOKEN"] = SENTINEL
    with RustainTUI(fresh=True, build=False, env_overrides=leaky) as tui:
        tui.send_message("scene ping")
        turns = _wait_for(lambda: _turn_rows(stub))
        assert len(turns) == 1, stub.rows()
        assert turns[0]["auth"] == "bearer"
        assert turns[0]["persona"] == "<unknown>"
        assert turns[0]["desync"] is True

    log_text = stub.log_path.read_text()
    assert log_text.count(SENTINEL) == 0
    assert SENTINEL not in json.dumps(stub.rows())

    # Half 2 — the A5 override in force, on a second stub so the statuses stay
    # independent: the scheme and the persona are named, and nothing else is.
    clean = SceneStub(SCENE, tmp_path / "clean.jsonl").start()
    home2 = tmp_path / "home2"
    home2.mkdir()
    try:
        with RustainTUI(fresh=True, build=False, env_overrides=clean.env("scene-ci", home2)) as tui:
            tui.send_message("scene ping")
            turns = _wait_for(lambda: _turn_rows(clean))
            assert len(turns) == 1, clean.rows()
            assert turns[0]["auth"] == "x-api-key"
            assert turns[0]["persona"] == "scene-ci"
            assert turns[0]["desync"] is False
    finally:
        assert clean.stop() == 0
    assert SENTINEL not in clean.log_path.read_text()


def test_the_stub_carries_no_shell_trace_switch():
    """AC2(d): a `set -x` in the provider would put env values in a receipt."""
    assert "set -x" not in (Path(__file__).parent / "fixtures" / "scene_provider.py").read_text()


# ── AC3 — the harness drives any binary it is told to ────────────────────────


def test_missing_binary_override_raises_file_not_found(monkeypatch, tmp_path):
    """AC3(a): a named binary that is absent fails by TYPE, before build and spawn.

    Without the existence check, `pexpect.spawn` raises `ExceptionPexpect` — also
    early, also naming the path — so a type-less "raises" assertion would stay green
    with the check deleted.
    """
    missing = tmp_path / "no-such-rustain"
    monkeypatch.setenv("RUSTAIN_TUI_BINARY", str(missing))
    harness = RustainTUI(fresh=True, build=True)
    with pytest.raises(FileNotFoundError) as excinfo:
        harness.start()
    assert "RUSTAIN_TUI_BINARY=" in str(excinfo.value)
    assert str(missing) in str(excinfo.value)


def test_non_tui_binary_override_fails_as_a_dead_process(monkeypatch):
    """AC3(a) discriminator: an existing non-TUI binary is a DIFFERENT failure.

    Measured, correcting the story's expectation that `start()` returns after the
    15 s `Ready` wait: the wait's first screen sync already finds the child gone, so
    `start()` itself raises `RuntimeError("TUI process is not alive…")` in well under
    a second. A `FileNotFoundError` here would mean the existence check fired on a
    path that exists — which is why this case is asserted separately.
    """
    monkeypatch.setenv("RUSTAIN_TUI_BINARY", "/bin/true")
    harness = RustainTUI(fresh=True, build=False)
    try:
        with pytest.raises(RuntimeError, match="not alive"):
            harness.start()
    finally:
        harness.stop()


@pytest.mark.skipif(
    not RELEASE_BINARY.exists(),
    reason="release binary not built — run cargo build --release --features p2p,a2a",
)
def test_release_binary_override_drives_a_turn(monkeypatch, stub, scratch_home):
    """AC3(a) positive control: the release-profile binary, which is 19.13's path."""
    monkeypatch.setenv("RUSTAIN_TUI_BINARY", str(RELEASE_BINARY))
    with RustainTUI(fresh=True, build=True, env_overrides=stub.env("scene-ci", scratch_home)) as tui:
        tui.send_message("scene ping")
        assert tui.wait_for_screen("Ready", timeout=60) is True, tui.get_screen_text()
        turns = _wait_for(lambda: _turn_rows(stub))
        assert len(turns) == 1 and turns[0]["desync"] is False, stub.rows()
        rows = _wait_for(lambda: _ledger_rows(tui))
        assert rows and rows[0]["usage"]["tokensOut"] >= 1, rows


def test_the_plain_script_precedent_runs_green():
    """AC3(c): `manual_test_ledger.py` — the launcher 19.8's drivers copy."""
    result = subprocess.run(
        [sys.executable, "tests_tui/manual_test_ledger.py"],
        cwd=str(PROJECT_ROOT),
        capture_output=True,
        text=True,
        timeout=300,
        env={k: v for k, v in os.environ.items() if k != "RUSTAIN_TUI_BINARY"},
    )
    assert result.returncode == 0, result.stdout + result.stderr
    assert "ledger row(s) from this run's workspace" in result.stdout
    assert "tokensOut=7" in result.stdout, result.stdout
