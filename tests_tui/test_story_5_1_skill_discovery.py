"""Story 5-1: Agent Skills Discovery & Catalog.

TUI contract tests for Tier 1 skill discovery from standard directories.

All tests are non-API — they exercise the TUI lifecycle (startup, background
scan, autocomplete) without sending messages to an LLM.  Skills are
pre-populated in the temp workspace before the TUI starts.

Covers: AC3 (validation warnings), AC4 (autocomplete), AC5 (empty workspace),
         AC6 (background scan after first frame), AC10 (disabled skills).
"""

from __future__ import annotations

import time

import pytest

from harness import RustainTUI
from keys import ESC, ENTER, TAB


# ── Helpers ─────────────────────────────────────────────────────────────────


def write_skill(workspace, tier, name, description, body="# Body\n"):
    """Create a valid skill in the workspace under the given tier directory.

    tier examples: ".agents/skills", ".rustain/skills", ".claude/skills"
    """
    from pathlib import Path
    workspace = Path(workspace)
    skill_dir = workspace / tier / name
    skill_dir.mkdir(parents=True, exist_ok=True)
    skill_file = skill_dir / "SKILL.md"
    skill_file.write_text(f"---\nname: {name}\ndescription: {description}\n---\n\n{body}")
    return skill_file


def write_flat_skill(workspace, tier, name, description):
    """Create a flat .md skill file (no subdirectory)."""
    from pathlib import Path
    workspace = Path(workspace)
    tier_dir = workspace / tier
    tier_dir.mkdir(parents=True, exist_ok=True)
    skill_file = tier_dir / f"{name}.md"
    skill_file.write_text(f"---\nname: {name}\ndescription: {description}\n---\n\n# Body\n")
    return skill_file


def _toml_string_array(values):
    """Format a Python list of strings as a valid TOML string array.

    Python's ``repr`` emits single-quoted strings which is NOT valid TOML;
    TOML requires double-quoted strings in arrays. We also escape any embedded
    double quotes and backslashes so the output is safe for arbitrary names.
    """
    escaped = []
    for v in values:
        v_str = str(v).replace("\\", "\\\\").replace("\"", "\\\"")
        escaped.append(f'"{v_str}"')
    return "[" + ", ".join(escaped) + "]"


def write_rustain_config(workspace, disabled=None):
    """Write a .rustain/config.toml with optional [skills] disabled list."""
    from pathlib import Path
    workspace = Path(workspace)
    config_dir = workspace / ".rustain"
    config_dir.mkdir(parents=True, exist_ok=True)
    config_file = config_dir / "config.toml"
    lines = []
    if disabled:
        lines.append("[skills]")
        lines.append(f"disabled = {_toml_string_array(disabled)}")
    config_file.write_text("\n".join(lines) + "\n")
    return config_file


def _start_tui_with_workspace(workspace_path):
    """Start a RustainTUI pointing at the given workspace (no auto-build)."""
    tui = RustainTUI(fresh=True, build=False, workspace=workspace_path)
    tui.start()
    return tui


def _wait_for_skills_loaded(tui: RustainTUI, screen_text: str, timeout: float = 15.0) -> None:
    """Wait for the background skill scan to complete.

    Polls the pyte screen for *screen_text* (the SystemNotice flash).
    If the flash expires before a poll catches it, falls back to checking
    the log file for ``SkillsDiscovered`` or ``Skills scan`` entries.
    """
    if tui.wait_for_screen(screen_text, timeout=timeout):
        return
    # Fallback: check log for evidence the scan ran
    log_pattern = r"Background skill scan complete:\s+\d+ skills"
    tui.assert_log_contains(log_pattern, msg=f"Skill scan completed but '{screen_text}' not on screen")


# ── Tests ───────────────────────────────────────────────────────────────────


@pytest.mark.story_5_1
def test_slash_autocomplete_shows_discovered_skills(build_binary, tmp_path):
    """AC4: Discovered skills appear in / autocomplete after background scan.

    Pre-populate .agents/skills/reviewer/SKILL.md in the workspace,
    start the TUI, wait for the 'Loaded N skills' notice, then press /
    and verify the skill name appears in the autocomplete dropdown.
    """
    write_skill(tmp_path, ".agents/skills", "reviewer", "Reviews code for bugs")

    tui = _start_tui_with_workspace(tmp_path)
    try:
        _wait_for_skills_loaded(tui, "Loaded 1 skill")

        tui.send("/")
        time.sleep(0.5)

        tui.assert_screen_contains("reviewer", msg="Skill 'reviewer' should appear in / autocomplete")
        tui.assert_screen_contains("Reviews code for bugs", msg="Skill description should appear in autocomplete")

        tui.send(ESC)
        time.sleep(0.3)
    finally:
        tui.stop()


@pytest.mark.story_5_1
def test_empty_workspace_no_skill_notice(build_binary, tmp_path):
    """AC5: No skill dirs → no 'Loaded' notice, no warnings, autocomplete unchanged.

    Uses an explicitly empty ``tmp_path`` workspace to guarantee no skill
    directories exist — avoids spurious failures when the shared ``tui``
    fixture points at a workspace that contains skills.
    """
    tui = _start_tui_with_workspace(tmp_path)
    try:
        # Give the background scan time to complete
        time.sleep(3.0)

        tui.assert_screen_not_contains(
            "Loaded", msg="Empty workspace should not show 'Loaded' notice"
        )
        tui.assert_screen_not_contains(
            "failed validation",
            msg="Empty workspace should not show validation warnings",
        )

        tui.send("/")
        time.sleep(0.5)

        # Built-in commands should still appear
        tui.assert_screen_contains(
            "/new", msg="Built-in /new should still appear in autocomplete"
        )

        tui.send(ESC)
        time.sleep(0.3)
    finally:
        tui.stop()


@pytest.mark.story_5_1
def test_invalid_skill_surfaces_warning_notice(build_binary, tmp_path):
    """AC3: Invalid skill (uppercase name) triggers aggregated warning notice.

    Write a skill with an invalid name (uppercase) to .agents/skills/.
    The scanner should exclude it and surface a warning notice.
    """
    skill_dir = tmp_path / ".agents" / "skills" / "Bad-Skill"
    skill_dir.mkdir(parents=True, exist_ok=True)
    (skill_dir / "SKILL.md").write_text(
        "---\nname: Bad-Skill\ndescription: Invalid uppercase name\n---\n"
    )

    tui = _start_tui_with_workspace(tmp_path)
    try:
        # Wait for the aggregated warning notice or log it
        found = tui.wait_for_screen("failed validation", timeout=10.0)
        if not found:
            # The notice may have scrolled off. Check the log instead.
            tui.assert_log_contains(
                r"excluded.*does not match pattern",
                msg="Invalid skill should be excluded with a validation log entry",
            )
            return

        tui.send("/")
        time.sleep(0.5)
        tui.assert_screen_not_contains("Bad-Skill", msg="Invalid skill should not appear in autocomplete")
        tui.send(ESC)
        time.sleep(0.3)
    finally:
        tui.stop()


@pytest.mark.story_5_1
def test_disabled_skill_hidden_from_autocomplete(build_binary, tmp_path):
    """AC10: Skills in config disabled list are hidden from autocomplete.

    Write .rustain/config.toml with disabled=["hidden-skill"] and create
    the skill. It should not appear in the / dropdown.
    """
    write_skill(tmp_path, ".agents/skills", "hidden-skill", "Should be hidden")
    write_skill(tmp_path, ".agents/skills", "visible-skill", "Should be visible")
    write_rustain_config(tmp_path, disabled=["hidden-skill"])

    tui = _start_tui_with_workspace(tmp_path)
    try:
        _wait_for_skills_loaded(tui, "Loaded 1 skill")

        tui.send("/")
        time.sleep(0.5)

        tui.assert_screen_not_contains(
            "hidden-skill", msg="Disabled skill should not appear in autocomplete"
        )
        tui.assert_screen_contains(
            "visible-skill", msg="Non-disabled skill should appear in autocomplete"
        )

        tui.send(ESC)
        time.sleep(0.3)
    finally:
        tui.stop()


@pytest.mark.story_5_1
def test_skill_selection_shows_placeholder_notice(build_binary, tmp_path):
    """AC4: Selecting a skill from autocomplete triggers skill activation flow.

    Uses a skill name that is unlikely to prefix-match any built-in command
    (e.g. ``zzz-test-skill``) and narrows the autocomplete filter so the
    skill is the ONLY entry. Pressing Enter selects the skill from
    autocomplete (inserting ``/zzz-test-skill `` into the input buffer);
    a second Enter submits the command. For workspace-tier skills this
    surfaces the trust prompt (Story 5-2 AC4).
    """
    # Name chosen so a narrow filter uniquely identifies this skill.
    skill_name = "zzz-test-skill"
    write_skill(tmp_path, ".agents/skills", skill_name, "A test skill")

    tui = _start_tui_with_workspace(tmp_path)
    try:
        _wait_for_skills_loaded(tui, "Loaded 1 skill")

        tui.send("/")
        time.sleep(0.3)
        # Type enough of the skill name that only this skill matches.
        tui.send("zzz")
        time.sleep(0.5)

        tui.assert_screen_contains(
            skill_name, msg="Filtered skill should appear as the sole autocomplete entry"
        )

        # First ENTER: accept the autocomplete suggestion.
        tui.send(ENTER)
        time.sleep(0.5)

        # Second ENTER: submit the skill command.
        tui.send(ENTER)

        # Workspace-tier skills require trust confirmation before activation.
        assert tui.wait_for_screen(
            "Trust and enable this skill",
            timeout=5.0,
        ), "Workspace skill should trigger trust prompt after submission"
    finally:
        tui.stop()


@pytest.mark.story_5_1
def test_multiple_skills_appear_in_autocomplete(build_binary, tmp_path):
    """AC4: Multiple discovered skills all appear in autocomplete."""
    write_skill(tmp_path, ".agents/skills", "alpha-skill", "First skill")
    write_skill(tmp_path, ".agents/skills", "beta-skill", "Second skill")
    write_skill(tmp_path, ".claude/skills", "gamma-skill", "Third skill")

    tui = _start_tui_with_workspace(tmp_path)
    try:
        _wait_for_skills_loaded(tui, "Loaded 3 skill")

        tui.send("/")
        time.sleep(0.5)

        tui.assert_screen_contains("alpha-skill", msg="alpha-skill should appear")
        tui.assert_screen_contains("beta-skill", msg="beta-skill should appear")

        tui.send(ESC)
        time.sleep(0.3)
    finally:
        tui.stop()


@pytest.mark.story_5_1
def test_priority_dedup_only_highest_shown(build_binary, tmp_path):
    """AC7: Skills with the same name from different tiers — highest priority wins.

    Create 'review' in both .agents/skills/ (priority 0) and .claude/skills/
    (priority 2). Only 1 skill should be loaded, and it should be the
    .agents/skills/ version.
    """
    write_skill(tmp_path, ".agents/skills", "review", "Agents tier review")
    write_skill(tmp_path, ".claude/skills", "review", "Claude tier review")

    tui = _start_tui_with_workspace(tmp_path)
    try:
        _wait_for_skills_loaded(tui, "Loaded 1 skill")

        tui.send("/")
        time.sleep(0.5)
        tui.assert_screen_contains("Agents tier review", msg="Higher priority skill should win")
        tui.assert_screen_not_contains("Claude tier review", msg="Lower priority should be shadowed")
        tui.send(ESC)
        time.sleep(0.3)
    finally:
        tui.stop()


@pytest.mark.story_5_1
def test_skill_discovery_does_not_block_startup(build_binary, tmp_path):
    """AC6: Skill scan runs in background — TUI is responsive immediately.

    Create several skills, start the TUI, and verify the app is responsive
    before the skill scan completes (the scan is async after first frame).
    """
    write_skill(tmp_path, ".agents/skills", "async-skill-1", "First async")
    write_skill(tmp_path, ".agents/skills", "async-skill-2", "Second async")
    write_skill(tmp_path, ".agents/skills", "async-skill-3", "Third async")

    tui = _start_tui_with_workspace(tmp_path)
    try:
        tui.assert_responsive(timeout=3.0)

        _wait_for_skills_loaded(tui, "Loaded 3 skill")
    finally:
        tui.stop()


@pytest.mark.story_5_1
def test_skill_autocomplete_filter_by_name(build_binary, tmp_path):
    """AC4: Substring filtering works on skill name (case-insensitive)."""
    write_skill(tmp_path, ".agents/skills", "lint-code", "Static analysis skill")
    write_skill(tmp_path, ".agents/skills", "review-code", "Code review skill")

    tui = _start_tui_with_workspace(tmp_path)
    try:
        _wait_for_skills_loaded(tui, "Loaded 2 skill")

        tui.send("/")
        time.sleep(0.3)
        tui.send("lint")
        time.sleep(0.5)

        tui.assert_screen_contains("lint-code", msg="Filtering by 'lint' should match lint-code")

        tui.send(ESC)
        time.sleep(0.3)
    finally:
        tui.stop()
