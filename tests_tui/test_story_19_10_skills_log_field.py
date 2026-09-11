"""Story 19.10 AC8 — skill and tool-name request-log evidence.

Tier 2 of FR41 is *"the skill's instructions reach the model on activation"*.
The `Inspect` overlay proves a body reached the **operator**; the Anthropic
scene-turn row records the canonical skill-block names the binary put on the
wire:

    "skills":["safe-deploy"]

The review also requires identity, not cardinality, for the offered catalogue.
Story 19.11 makes the enforced Bash pattern visible as the bare tool:

    "tool_names":["Bash","Read","activate_skill","task"]

Both fields contain names only—never skill bodies, the system prompt, tool
schemas, or tool descriptions. Tests below reject tag-like persona prose,

The committed J3 fixture is the PRD's own `SKILL.md`, byte for byte, copied by
``fixtures.skills.write_workspace_skill``, so this control and the capture agree
on what a skill is.

Run:
    pytest tests_tui/test_story_19_10_skills_log_field.py -m "not requires_api"
"""

from __future__ import annotations

import sys
import time
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).parent))

from fixtures.scene_provider import SceneStub, _skill_names
from fixtures.skills import write_workspace_skill
from harness import RustainTUI

SCENE = Path(__file__).parent / "fixtures" / "scenes" / "skills_log_field.json"
OPENAI_SCENE = Path(__file__).parent / "fixtures" / "scenes" / "openai_wire.json"
SKILL_SRC = (
    Path(__file__).parent / "journeys" / "j3" / "skills" / "safe-deploy" / "SKILL.md"
)
PERSONA = "scene-ci-skills"
SKILL_NAME = "safe-deploy"
FILTERED_TOOL_NAMES = ["Bash", "Read", "activate_skill", "task"]

pytestmark = pytest.mark.story_19_10


# ── Helpers ──────────────────────────────────────────────────────────────────


def _turn_rows(stub: SceneStub) -> list[dict]:
    """Scene-turn rows only: boot/probe/title rows carry `kind` and no `turn`."""
    return [row for row in stub.rows() if "turn" in row and "kind" not in row]


def _wait_for(predicate, timeout: float = 20.0, interval: float = 0.25):
    deadline = time.monotonic() + timeout
    value = predicate()
    while not value and time.monotonic() < deadline:
        time.sleep(interval)
        value = predicate()
    return value


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


def _tui(stub, workspace: Path, scratch_home: Path) -> RustainTUI:
    """An UNSTARTED harness — `RustainTUI.__enter__` is what calls `start()`."""
    return RustainTUI(
        fresh=True,
        build=False,
        workspace=workspace,
        allowed_tools=[],
        env_overrides=stub.env(PERSONA, scratch_home),
        timeout=60,
    )


def _drop_copied_credential(workspace: Path) -> None:
    """`harness.start()` copies the developer's real `rustain/.env` in."""
    (workspace / ".env").unlink(missing_ok=True)


def _activate(tui: RustainTUI) -> None:
    """Drive the model-driven activation through the trust gate with `y`.

    ⛔ Not `/safe-deploy`: that is the user-driven slash route
    (`event_loop.rs:7733`), a different production path. The scene emits a real
    `activate_skill` tool_use, which is the door `execute_activate_skill`
    (`toolset_adapter.rs:1604`) opens and the one the trust gate guards.
    """
    tui.send_message("use the deploy skill")
    assert tui.wait_for_screen(
        "Trust and enable this skill for this session?", timeout=45
    ), tui.get_screen_text()
    tui.send("y")
    assert tui.wait_for_screen("protocol loaded", timeout=60), tui.get_screen_text()



def test_tag_like_persona_text_is_not_skill_evidence():
    """A persona mentioning the tag syntax did not activate a skill."""
    body = {
        "system": 'Explain the text <skill name="audit-marker"> without loading it.'
    }
    assert _skill_names(body) == []


def test_canonical_trailing_skill_block_logs_activated_name():
    body = {
        "system": (
            "persona\n"
            '<skill name="safe-deploy" source="WorkspaceAgents">\n'
            "<instructions>\nDeploy safely.\n</instructions>\n"
            "<skill_directory>/workspace/.agents/skills/safe-deploy</skill_directory>\n"
            "<workspace_root>/workspace</workspace_root>\n"
            "</skill>"
        )
    }
    assert _skill_names(body) == ["safe-deploy"]

# ── AC8(b) — the field, both ways round ──────────────────────────────────────


def test_a_turn_with_no_active_skill_logs_no_skill_names(stub, scratch_home, workspace):
    """The negative half: an empty activation set composes no `<skill>` block, so
    the field is present and empty rather than absent-and-ambiguous."""
    write_workspace_skill(workspace, SKILL_SRC)
    with _tui(stub, workspace, scratch_home) as tui:
        _drop_copied_credential(workspace)
        tui.send_message("no skill yet")
        assert tui.wait_for_screen("nothing active", timeout=45), tui.get_screen_text()

        rows = _wait_for(lambda: _turn_rows(stub))
        assert len(rows) == 1, stub.rows()
        assert rows[0]["skills"] == [], rows[0]
        assert len(rows[0]["tool_names"]) == rows[0]["tools"], rows[0]


def test_a_skill_active_turn_logs_exactly_the_activated_name(
    stub, scratch_home, workspace
):
    """The positive half, and the mechanism's first non-test reader: after a
    model-driven `activate_skill` the next POST's system prompt carries the
    block, and the row names it — exactly one name, exactly the activated one."""
    write_workspace_skill(workspace, SKILL_SRC)
    with _tui(stub, workspace, scratch_home) as tui:
        _drop_copied_credential(workspace)
        tui.send_message("no skill yet")
        assert tui.wait_for_screen("nothing active", timeout=45), tui.get_screen_text()
        _activate(tui)

        # A14: the catalogue, the system prompt and the disclosure are computed
        # ONCE per `submit`, from the activation set as it stood BEFORE the call.
        # The turn that activates the skill therefore still carries the
        # pre-activation prompt; the field only fills on the NEXT submission.
        tui.send_message("second message")
        assert tui.wait_for_screen("still following it", timeout=60), (
            tui.get_screen_text()
        )

        rows = _wait_for(lambda: [r for r in _turn_rows(stub) if r["skills"]])
        assert rows, [r for r in _turn_rows(stub)]
        assert all(r["skills"] == [SKILL_NAME] for r in rows), rows
        assert all(r["tool_names"] == FILTERED_TOOL_NAMES for r in rows), rows
        # And the activation turn itself is still pre-activation (A14/A15).
        first = _turn_rows(stub)[0]
        assert first["skills"] == [], first


def test_the_field_never_carries_the_skill_body(stub, scratch_home, workspace):
    """The leak guard. `helm diff` is on line 2 of the PRD's Protocol and appears
    nowhere in its `name:`, so a field that ever carries the body — or the whole
    system prompt — fails here rather than in a committed receipt."""
    write_workspace_skill(workspace, SKILL_SRC)
    assert "helm diff" in SKILL_SRC.read_text(), "fixture drifted from the PRD body"
    with _tui(stub, workspace, scratch_home) as tui:
        _drop_copied_credential(workspace)
        tui.send_message("no skill yet")
        assert tui.wait_for_screen("nothing active", timeout=45), tui.get_screen_text()
        _activate(tui)
        tui.send_message("second message")
        assert tui.wait_for_screen("still following it", timeout=60), (
            tui.get_screen_text()
        )

        rows = _wait_for(lambda: [r for r in _turn_rows(stub) if r["skills"]])
        assert rows, [r for r in _turn_rows(stub)]
        for row in _turn_rows(stub):
            for name in row["skills"]:
                assert "helm diff" not in name, row
                assert "## Protocol" not in name, row
                assert len(name) <= 64, row
        # …and nowhere else in the log either: the whole file is committed to a
        # receipt directory in the J3 capture.
        assert "helm diff" not in stub.log_path.read_text(encoding="utf-8")


# ── AC8(c) — the OpenAI arm is untouched ─────────────────────────────────────


def test_the_openai_arm_carries_no_skills_field(tmp_path):
    """AC8(c). The second wire serves text turns and composes no skill blocks, so
    its rows gain no field: `skills` is an Anthropic-arm addition or it is a
    schema change to a wire this story never exercised."""
    with SceneStub(OPENAI_SCENE, tmp_path / "requests.jsonl") as instance:
        import json
        import urllib.request

        request = urllib.request.Request(
            f"{instance.url}/v1/chat/completions",
            data=json.dumps(
                {
                    "model": "anthropic/claude-sonnet-4.6",
                    "stream": True,
                    "messages": [{"role": "user", "content": "other wire ping"}],
                }
            ).encode(),
            headers={
                "authorization": "Bearer scene-ci-or",
                "content-type": "application/json",
            },
        )
        with urllib.request.urlopen(request, timeout=15) as response:
            response.read()

        rows = [row for row in instance.rows() if row.get("wire") == "openai"]
        assert len(rows) == 1, instance.rows()
        assert "skills" not in rows[0], rows[0]
        assert "tool_names" not in rows[0], rows[0]
