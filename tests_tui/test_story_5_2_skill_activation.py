"""Story 5-2: Agent Skills Progressive Disclosure & Execution.

TUI contract tests for non-API-driven user interactions:
 - autocomplete insertion (AC8)
 - workspace-tier trust prompt appearance + key handling (AC4)

Full activation + turn-kickoff tests (AC1/AC6/AC8/AC12 end-to-end) require a
real LLM provider and are covered by the Rust integration suite
(``tests/skill_activation.rs``).  This file focuses on TUI mechanics that are
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

from harness import RustainTUI, composer_top
from keys import ESC


CHAR_DELAY = 0.015  # seconds per keystroke — matches harness.send_message default


# ── Helpers ────────────────────────────────────────────────────────────────


def type_slowly(t: RustainTUI, text: str, delay: float = CHAR_DELAY) -> None:
    """Type one character at a time — the raw-mode TUI drops burst input."""
    for c in text:
        t.send(c)
        time.sleep(delay)


def settle(t: RustainTUI, timeout: float = 30.0, duration: float = 0.7) -> str:
    """Decision keys are inert for 500 ms after a frame's first paint (Story 21.6): wait
    structurally until the whole screen has stayed unchanged for ``duration``."""
    previous, since = None, None
    deadline = time.monotonic() + timeout
    while True:
        screen = t.get_screen_text()
        now = time.monotonic()
        assert now < deadline, screen
        if screen != previous:
            previous, since = screen, now
        elif now - since >= duration:
            return screen


def reopen_from_list(t: RustainTUI, body_text: str) -> None:
    """`Ctrl+X q` opens the Needs-you list, `Enter` opens the selected deferred decision."""
    t.send("\x18q")
    assert t.wait_for_screen("[Enter] Open", timeout=5.0), t.get_screen_text()
    settle(t)
    t.send("\r")
    assert t.wait_for_screen(body_text, timeout=5.0), t.get_screen_text()


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


def _popup(screen: str) -> str:
    """The rows above the composer box (the autocomplete popup and the chat), never the
    composer's own typed text."""
    lines = screen.split("\n")
    top = composer_top(lines)
    return "\n".join(lines[:top]) if top is not None else ""


def _composer_text(screen: str) -> str:
    """The composer box (its top border down to the screen's last row)."""
    lines = screen.split("\n")
    top = composer_top(lines)
    return "\n".join(lines[top:]) if top is not None else ""


# ── Tests ──────────────────────────────────────────────────────────────────


@pytest.mark.story_5_2
def test_workspace_skill_triggers_trust_prompt(tmp_path):
    """AC4/AC8: a workspace skill is listed by the slash popup; accepting and submitting it shows
    the ``Decision: Skill trust`` frame; Esc defers it (the frame closes, the ``Needs you (1)``
    chip waits, nothing is declined); ``i`` inspects the skill file and Esc returns to the frame;
    ``n`` declines (Info notice, frame gone). ``y`` is never pressed: activation starts a turn.

    21.6: the prompt is the `Decision: Skill trust` frame; its keys are generated from the keymap.
    """
    ws = tmp_path / "ws"
    home = tmp_path / "home"
    ws.mkdir()
    home.mkdir()
    write_workspace_skill(
        ws,
        "zzz-trust-skill",
        description="Trust me",
        body="# Unique Inspect Marker 7c4f\nContent body.\n",
    )
    t = RustainTUI(build=False, workspace=ws, env_overrides={"HOME": str(home)})
    t.start()
    try:
        # L1: the skill is listed, accepted, submitted; the trust frame opens.
        type_slowly(t, "/zzz")
        t.wait_until(lambda s: "zzz-trust-skill" in _popup(s), "the workspace skill in the slash popup")
        t.send("\r")
        t.wait_until(lambda s: "/zzz-trust-skill" in _composer_text(s), "the accepted skill name in the composer")
        t.send("\r")
        assert t.wait_for_screen("Decision: Skill trust", timeout=5.0), t.get_screen_text()
        settle(t)

        # L2: Esc defers the decision: the frame closes, the chip waits, nothing is declined.
        t.send(ESC)
        t.wait_until(lambda s: "Decision: Skill trust" not in s and "Needs you (1)" in s,
                     "the frame closed with the Needs you (1) chip")
        reopen_from_list(t, "Decision: Skill trust")
        settle(t)

        # L3: inspect shows the skill file; Esc returns to the frame.
        t.send("i")
        assert t.wait_for_screen("Unique Inspect Marker 7c4f", timeout=3.0), t.get_screen_text()
        settle(t)
        t.send(ESC)
        t.wait_until(lambda s: "Unique Inspect Marker 7c4f" not in s and "Decision: Skill trust" in s,
                     "the trust frame after the inspect view closed")
        settle(t)

        # L4: the explicit decline is the only decline.
        mark = t.notification_mark()
        t.send("n")
        t.wait_for_notification("not trusted", mark, "Info")
        assert t.wait_for_screen_not_contains("Decision: Skill trust", timeout=3.0), t.get_screen_text()
    finally:
        t.stop()
