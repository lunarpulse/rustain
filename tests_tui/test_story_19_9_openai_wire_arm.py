"""Story 19.9 AC8 — the scene provider's OpenAI-compat wire, through the binary.

The 19.7 stub speaks the Anthropic Messages wire only; PRD Journey 2's provider
switch runs on the OpenAI-compat adapter (`POST {base_url}/chat/completions`,
`src/adapters/openai/mod.rs`), so the stub gained a second arm in Story 19.9
(ruling A11). This file is that arm's positive control in the **non-API** lane:
it drives the real binary down the **config** provider path with
``[provider.openrouter] base_url = "<stub>/v1"`` and asserts on what the binary
sent, journaled and rendered — never on prose the stub was told to serve.

⛔ The frame shape under test is pinned from the ADAPTER, not from OpenAI's docs:
``object`` is a required field of ``OpenAiStreamEvent``, ``finish_reason: "stop"``
is what mints ``TurnComplete``, and ``usage`` rides a final ``choices: []`` chunk
because ``OpenAiRequest::from`` always sets ``stream_options.include_usage``.
Each of those three is covered by a mutant recorded in the story's Debug Log.

Run:
    pytest tests_tui/test_story_19_9_openai_wire_arm.py -m "not requires_api"
"""

from __future__ import annotations

import json
import sys
import time
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).parent))

from fixtures.scene_provider import SceneStub
from harness import RustainTUI

SCENE = Path(__file__).parent / "fixtures" / "scenes" / "openai_wire.json"
OR_PERSONA = "scene-ci-or"
OR_MODEL = "anthropic/claude-sonnet-4.6"

pytestmark = pytest.mark.story_19_9


# ── Helpers ──────────────────────────────────────────────────────────────────


def _write_config(workspace: Path, stub_url: str, *, model_id: str = OR_MODEL) -> None:
    """The config provider path: `[provider.openrouter]` pointed at the stub.

    Written BEFORE `RustainTUI.start()`, which only creates `.rustain/config.toml`
    when it is absent — so this block, not the harness default, is what
    `init_provider_layer` reads. An empty `always_tools` keeps the permission
    gate real, matching the journey drivers.
    """
    rustain_dir = workspace / ".rustain"
    rustain_dir.mkdir(parents=True, exist_ok=True)
    (rustain_dir / "config.toml").write_text(
        "[permissions]\n"
        "always_tools = []\n"
        "\n"
        "[provider.openrouter]\n"
        'kind = "openrouter"\n'
        'provider_id = "openrouter"\n'
        f'model_id = "{model_id}"\n'
        'api_key_env = "OPENROUTER_API_KEY"\n'
        f'base_url = "{stub_url}/v1"\n'
        "enabled = true\n"
    )


def _env(stub_url: str, home: Path, *, key: str) -> dict[str, str]:
    """The 19.7 env block MINUS the Anthropic base URL and key.

    Setting both would put two front doors in one capture; the config `base_url`
    is what this test exercises. `ANTHROPIC_AUTH_TOKEN` is blanked because the
    harness copies `rustain/.env`, which exports a real one.
    """
    return {
        "ANTHROPIC_AUTH_TOKEN": "",
        "ANTHROPIC_API_KEY": "",
        "ANTHROPIC_BASE_URL": "",
        "ANTHROPIC_DEFAULT_SONNET_MODEL": "",
        "RUSTAIN_MODELS_DEV_URL": stub_url,
        "OPENROUTER_API_KEY": key,
        "HOME": str(home),
        "NO_COLOR": "1",
    }


def _ledger_rows(tui: RustainTUI) -> list[dict]:
    usage_dir = tui.wp / ".rustain_data" / "usage"
    if not usage_dir.exists():
        return []
    return [
        json.loads(line)
        for path in sorted(usage_dir.glob("*.jsonl"))
        for line in path.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]


def _wait_for(predicate, timeout: float = 15.0, interval: float = 0.25):
    deadline = time.monotonic() + timeout
    value = predicate()
    while not value and time.monotonic() < deadline:
        time.sleep(interval)
        value = predicate()
    return value


def _openai_rows(stub: SceneStub) -> list[dict]:
    return [row for row in stub.rows() if row.get("wire") == "openai"]


# ── Fixtures ─────────────────────────────────────────────────────────────────


@pytest.fixture
def stub(tmp_path):
    """A scene provider whose exit status is itself an assertion (19.7 shape)."""
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


@pytest.fixture
def workspace(tmp_path) -> Path:
    ws = tmp_path / "workspace"
    ws.mkdir()
    return ws


# ── AC8(b) — one text turn on the second wire, end to end ────────────────────


def test_config_path_text_turn_streams_on_the_openai_wire(stub, scratch_home, workspace):
    """The binary boots on the config path, health-checks, and streams a turn
    through `POST /v1/chat/completions` with a Bearer credential."""
    _write_config(workspace, stub.url)
    with RustainTUI(
        fresh=True,
        build=False,
        workspace=workspace,
        allowed_tools=[],
        env_overrides=_env(stub.url, scratch_home, key=OR_PERSONA),
        timeout=60,
    ) as tui:
        (workspace / ".env").unlink(missing_ok=True)
        tui.send_message("other wire ping")

        # Rendering positive control — the one served string this file asserts.
        assert tui.wait_for_screen("other wire pong", timeout=45), tui.get_screen_text()
        assert tui.wait_for_screen("Ready", timeout=30), tui.get_screen_text()
        tui.assert_screen_not_contains("Stream disconnected")

        rows = _openai_rows(stub)
        assert len(rows) == 1, stub.rows()
        row = rows[0]
        assert row["path"] == "/v1/chat/completions", row
        assert row["auth"] == "bearer", row
        assert row["persona"] == OR_PERSONA, row
        assert row["stream"] is True, row
        assert row["desync"] is False, row
        assert "other wire ping" in row["last_user"], row
        assert "anthropic/" in row["model"], row

        # The OpenAI health check is GET {base_url}/models — the same endpoint
        # the connectivity probe hits, so it logs `kind: "probe"`.
        assert any(r.get("kind") == "probe" for r in stub.rows()), stub.rows()
        # No Anthropic-wire turn happened: the config path replaced that door.
        assert not [r for r in stub.rows() if r.get("path") == "/v1/messages"], stub.rows()

        # Product-minted: `usage` on the final chunk is the only thing that can
        # put a non-zero tokensOut in the ledger on this wire.
        ledger = _wait_for(lambda: _ledger_rows(tui))
        assert len(ledger) == 1, ledger
        timestamp = ledger[0]["timestampMs"]
        assert isinstance(timestamp, int) and 10**12 <= timestamp < 10**13, ledger[0]
        assert ledger[0]["usage"]["tokensOut"] >= 1, ledger[0]
        assert "anthropic/" in ledger[0]["model"], ledger[0]


def test_undeclared_bearer_is_a_desync_and_never_reaches_the_log(
    stub, scratch_home, workspace
):
    """An undeclared Bearer value resolves to `<unknown>`, desyncs, and its bytes
    never enter the request log — the 19.7 A5 guarantee, on the second wire."""
    leak = "SENTINEL-LEAK-19-9"
    stub.expect_status = 1
    _write_config(workspace, stub.url)
    with RustainTUI(
        fresh=True,
        build=False,
        workspace=workspace,
        allowed_tools=[],
        env_overrides=_env(stub.url, scratch_home, key=leak),
        timeout=60,
    ) as tui:
        (workspace / ".env").unlink(missing_ok=True)
        tui.send_message("other wire ping")
        rows = _wait_for(lambda: _openai_rows(stub), timeout=45)

    assert rows, stub.rows()
    assert all(row["persona"] == "<unknown>" for row in rows), rows
    assert all(row["desync"] is True for row in rows), rows
    assert leak not in stub.log_path.read_text(encoding="utf-8")


def test_a_bearer_persona_on_the_anthropic_endpoint_is_a_wire_mismatch(stub):
    """A declared persona that reaches the other wire's endpoint desyncs rather
    than being served from the wrong turn list (the mis-route mutant, A11)."""
    import urllib.error
    import urllib.request

    stub.expect_status = 1
    request = urllib.request.Request(
        f"{stub.url}/v1/messages",
        data=json.dumps(
            {"stream": True, "model": "x", "messages": [{"role": "user", "content": "other wire ping"}]}
        ).encode(),
        headers={"content-type": "application/json", "x-api-key": OR_PERSONA},
        method="POST",
    )
    with urllib.request.urlopen(request, timeout=10) as response:  # noqa: S310
        body = response.read().decode()
    # The Anthropic wire signals a scene problem with its own `error` frame type
    # (`scene_desync`, as 19.7 pinned); the MESSAGE is what names the mismatch.
    # The `scene_wire_mismatch` type belongs to the OpenAI arm's 400, which has
    # no error-frame contract to reuse.
    assert '"type":"scene_desync"' in body, body
    assert "declared on the 'openai' wire" in body, body

    rows = [r for r in stub.rows() if r.get("turn") is not None]
    assert rows and rows[-1]["desync"] is True, stub.rows()
    assert rows[-1]["persona"] == OR_PERSONA, rows[-1]
    # The turn cursor did NOT advance: the openai persona's single turn is intact.
    assert rows[-1]["turn"] == 0, rows[-1]


def test_an_openai_persona_may_not_script_a_tool_use_turn(tmp_path):
    """AC8(a): text turns only in this cut — a scripted `tool_use` for a
    `wire: "openai"` persona is a load-time error (exit 2), not a served turn
    whose tool silently never runs."""
    import subprocess

    bad = tmp_path / "bad.json"
    bad.write_text(
        json.dumps(
            {
                "scene": "bad-openai",
                "personas": {
                    "scene-ci-or": {
                        "wire": "openai",
                        "turns": [
                            {
                                "expect": "x",
                                "content": [
                                    {"type": "tool_use", "name": "Read", "input": {"file_path": "a"}}
                                ],
                                "stop_reason": "tool_use",
                                "usage": {"input_tokens": 1, "output_tokens": 1},
                            }
                        ],
                    }
                },
            }
        )
    )
    provider = Path(__file__).parent / "fixtures" / "scene_provider.py"
    result = subprocess.run(  # noqa: S603
        [sys.executable, str(provider), "--scene", str(bad), "--port", "0",
         "--log", str(tmp_path / "log.jsonl")],
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert result.returncode == 2, result
    assert "TEXT" in result.stderr and "tool_use" in result.stderr, result.stderr
    assert "listening" not in result.stdout, result.stdout


def test_the_anthropic_serving_path_is_unchanged(stub, scratch_home, workspace):
    """Positive control for "additive": the same stub still serves the Anthropic
    wire byte-for-byte as 19.7 left it, in the same process and scene."""
    with RustainTUI(
        fresh=True,
        build=False,
        workspace=workspace,
        allowed_tools=[],
        env_overrides={
            **_env(stub.url, scratch_home, key=""),
            "ANTHROPIC_API_KEY": "scene-ci",
            "ANTHROPIC_BASE_URL": stub.url,
        },
        timeout=60,
    ) as tui:
        (workspace / ".env").unlink(missing_ok=True)
        tui.send_message("anthropic wire ping")
        assert tui.wait_for_screen("anthropic wire pong", timeout=45), tui.get_screen_text()

        rows = [r for r in stub.rows() if r.get("path") == "/v1/messages" and "turn" in r]
        assert len(rows) == 1, stub.rows()
        assert rows[0]["auth"] == "x-api-key", rows[0]
        assert rows[0]["persona"] == "scene-ci", rows[0]
        assert rows[0]["desync"] is False, rows[0]
        # The Anthropic arm logs no `wire` field — the row shape 19.7 pinned.
        assert "wire" not in rows[0], rows[0]
        assert not _openai_rows(stub), stub.rows()
