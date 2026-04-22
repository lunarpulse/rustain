"""Story 5-2: Agent Skills Progressive Disclosure & Execution.

TUI contract tests for non-API-driven user interactions:
 - autocomplete insertion (AC8)
 - workspace-tier trust prompt appearance + key handling (AC4)
 - `/deactivate` feedback messages (AC5)
 - status-bar `Skills: N active` segment visibility (AC12)

Full activation + turn-kickoff tests (AC1/AC6/AC8/AC12 end-to-end) require a
real LLM provider and are covered by the Rust integration suite
(`tests/skill_activation.rs`). This file focuses on TUI mechanics that are
observable without an API key.

Rewritten 2026-04-21 against the real ``tests_tui/harness.py`` API.
Raw-mode TUIs drop burst input from ``sendline()``, so all typing goes
through ``send_message`` (char-by-char with small delay) per the harness
convention (see ``harness.py::send_message`` docstring).
"""

from __future__ import annotations

import time
from pathlib import Path

import pytest

from harness import RustainTUI
from keys import ESC, TAB


CHAR_DELAY = 0.015  # seconds per keystroke — matches harness.send_message default


def type_slowly(t: RustainTUI, text: str, delay: float = CHAR_DELAY) -> None:
    """Type one character at a time — the raw-mode TUI drops burst input."""
    for c in text:
        t.send(c)
        time.sleep(delay)


def submit_slash(t: RustainTUI, cmd: str, delay: float = CHAR_DELAY) -> None:
    """Type ``cmd`` and dispatch it as a slash command.

    The TUI autocomplete intercepts the first Enter to accept the matching
    suggestion (rewriting the buffer and dismissing autocomplete).  A second
    Enter then submits through the normal handler.  This double-Enter pattern
    is the only reliable way to submit a slash command that has an
    autocomplete match — the autocomplete unconditionally captures Enter/Tab
    when active, consuming the event without submitting.

    ``submit_message`` parses via ``split_whitespace().next()`` which trims
    the trailing space injected by ``apply_autocomplete_selection``, so the
    command name is extracted correctly.
    """
    type_slowly(t, cmd, delay)
    time.sleep(0.2)
    t.send("\r")
    time.sleep(0.3)
    t.send("\r")


def write_workspace_skill(
    workspace: Path,
    name: str,
    description: str = "Test skill",
    body: str = "# Body\n",
    allowed_tools: list[str] | None = None,
) -> Path:
    """Write a ``.agents/skills/<name>/SKILL.md`` file under the workspace.

    The resulting skill lives in ``SkillSource::WorkspaceAgents``, so it requires
    a trust prompt before activation (AC4).
    """
    skill_dir = workspace / ".agents" / "skills" / name
    skill_dir.mkdir(parents=True, exist_ok=True)
    tools_line = ""
    if allowed_tools is not None:
        tools_yaml = ", ".join(f'"{tool}"' for tool in allowed_tools)
        tools_line = f"\nallowed-tools: [{tools_yaml}]"
    (skill_dir / "SKILL.md").write_text(
        f"---\nname: {name}\ndescription: {description}{tools_line}\n---\n\n{body}"
    )
    return skill_dir / "SKILL.md"


# ── Tests ──────────────────────────────────────────────────────────────────


@pytest.mark.story_5_2
def test_status_bar_no_skills_segment_when_none_active(tmp_path):
    """AC12 negative: status bar MUST NOT render ``Skills:`` segment at idle (0 active)."""
    ws = tmp_path / "ws"
    ws.mkdir()
    with RustainTUI(build=False, workspace=ws) as t:
        t.assert_screen_not_contains("Skills:")


@pytest.mark.story_5_2
def test_skill_appears_in_slash_autocomplete(tmp_path):
    """AC8: a discovered workspace skill shows up when the user types ``/``."""
    ws = tmp_path / "ws"
    ws.mkdir()
    write_workspace_skill(ws, "review-code", description="Review a Rust file")
    with RustainTUI(build=False, workspace=ws) as t:
        type_slowly(t, "/")
        assert t.wait_for_screen("review-code", timeout=5.0), (
            "workspace skill should appear in slash autocomplete"
        )


@pytest.mark.story_5_2
def test_workspace_skill_triggers_trust_prompt(tmp_path):
    """AC4: activating a workspace-tier skill shows the trust prompt with canonical text."""
    ws = tmp_path / "ws"
    ws.mkdir()
    write_workspace_skill(ws, "risky-skill")
    with RustainTUI(build=False, workspace=ws) as t:
        submit_slash(t, "/risky-skill")
        assert t.wait_for_screen("Trust and enable this skill", timeout=5.0)
        t.assert_screen_contains("[y]")
        t.assert_screen_contains("[n]")
        t.assert_screen_contains("[i]")


@pytest.mark.story_5_2
def test_trust_prompt_decline_via_n_key(tmp_path):
    """AC4: pressing ``n`` on the trust prompt aborts activation and emits the decline notice."""
    ws = tmp_path / "ws"
    ws.mkdir()
    write_workspace_skill(ws, "declined-skill")
    with RustainTUI(build=False, workspace=ws) as t:
        submit_slash(t, "/declined-skill")
        assert t.wait_for_screen("Trust and enable this skill", timeout=5.0)
        t.send("n")
        assert t.wait_for_screen("not trusted", timeout=3.0)


@pytest.mark.story_5_2
def test_trust_prompt_decline_via_esc(tmp_path):
    """AC4: Esc on the trust prompt is equivalent to ``n`` (decline)."""
    ws = tmp_path / "ws"
    ws.mkdir()
    write_workspace_skill(ws, "esc-decline")
    with RustainTUI(build=False, workspace=ws) as t:
        submit_slash(t, "/esc-decline")
        assert t.wait_for_screen("Trust and enable this skill", timeout=5.0)
        t.send(ESC)
        assert t.wait_for_screen("not trusted", timeout=3.0)


@pytest.mark.story_5_2
def test_trust_prompt_inspect_shows_file_content(tmp_path):
    """AC4: pressing ``i`` on the trust prompt opens inspect overlay with file contents."""
    ws = tmp_path / "ws"
    ws.mkdir()
    write_workspace_skill(
        ws,
        "inspectable",
        description="Inspect me",
        body="# Unique Inspect Marker 7c4f\nContent body.\n",
    )
    with RustainTUI(build=False, workspace=ws) as t:
        submit_slash(t, "/inspectable")
        assert t.wait_for_screen("Trust and enable this skill", timeout=5.0)
        t.send("i")
        assert t.wait_for_screen("Unique Inspect Marker 7c4f", timeout=3.0)
        t.send(ESC)
        assert t.wait_for_screen("Trust and enable this skill", timeout=3.0)


@pytest.mark.story_5_2
def test_deactivate_with_no_active_skills(tmp_path):
    """AC5: ``/deactivate`` on an empty active-set emits the canonical info notice."""
    ws = tmp_path / "ws"
    ws.mkdir()
    with RustainTUI(build=False, workspace=ws) as t:
        submit_slash(t, "/deactivate")
        assert t.wait_for_screen("No active skills to deactivate", timeout=5.0), (
            "AC5 canonical empty-set notice missing"
        )


@pytest.mark.story_5_2
def test_deactivate_unknown_skill_name(tmp_path):
    """AC5: ``/deactivate <name>`` for an inactive skill emits the 'not active' notice."""
    ws = tmp_path / "ws"
    ws.mkdir()
    with RustainTUI(build=False, workspace=ws) as t:
        # Typing the argument pushes the input past the palette-match window,
        # so a trailing Enter dispatches directly without needing submit_slash.
        type_slowly(t, "/deactivate unknown-skill")
        t.send("\r")
        assert t.wait_for_screen("is not active", timeout=5.0), (
            "AC5 unknown-skill notice missing"
        )
