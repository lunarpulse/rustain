"""Story 5-1: Agent Skills Discovery & Catalog.

TUI contract tests for Tier 1 skill discovery from standard directories.

All tests are non-API — they exercise the TUI lifecycle (startup, background
scan, autocomplete) without sending messages to an LLM.  Skills are
pre-populated in the temp workspace before the TUI starts.

Covers: AC3 (validation warnings), AC4 (autocomplete), AC7 (tier priority),
         AC10 (disabled skills).
"""

from __future__ import annotations

import time

import pytest

from harness import NotificationMark, RustainTUI, composer_top
from keys import CTRL_U, ESC


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


def _start_tui_with_workspace(workspace_path, isolated_home=None):
    """Start a RustainTUI pointing at the given workspace (no auto-build).

    ``isolated_home`` replaces ``$HOME`` so user-global skills in the developer's real home
    cannot add rows to the autocomplete popup or to the discovery notices.
    """
    overrides = {"HOME": str(isolated_home)} if isolated_home is not None else None
    tui = RustainTUI(fresh=True, build=False, workspace=workspace_path, env_overrides=overrides)
    tui.start()
    return tui


def _popup(screen: str) -> str:
    """The rows above the composer box: the autocomplete popup and the chat. Never the
    composer itself, so a typed filter token cannot satisfy a predicate."""
    lines = screen.split("\n")
    top = composer_top(lines)
    return "\n".join(lines[:top]) if top is not None else ""


def _filter(tui: RustainTUI, keys: str, shown, hidden=(), label: str = "") -> str:
    """Type ``keys`` and wait for a popup that lists every ``shown`` string and none of
    ``hidden`` in the same frame (a positive and an anchored negative)."""
    tui.send_keys(*keys, delay=0.05)
    return tui.wait_until(
        lambda s: all(x in _popup(s) for x in shown) and not any(x in _popup(s) for x in hidden),
        f"the popup after {keys!r} ({label})",
    )


def _reset(tui: RustainTUI, token: str, anchor: str) -> None:
    """Close the popup with ``Esc``, clear whatever it left in the composer with ``Ctrl+U`` and wait
    for the typed token to leave. Never sent with no popup open: ``Esc`` would move focus to Chat."""
    tui.send(ESC)
    tui.wait_until(lambda s: anchor not in _popup(s), f"the popup for {token} closed")
    tui.send(CTRL_U)
    assert tui.wait_for_screen_not_contains(token, timeout=5.0), tui.get_screen_text()


# ── Tests ───────────────────────────────────────────────────────────────────


@pytest.mark.story_5_1
def test_slash_autocomplete_shows_discovered_skills(build_binary, tmp_path):
    """AC3/AC4/AC7/AC10: one workspace, one launch; every popup leg reads the rows above the composer.

    A  ``rev``   lists ``reviewer`` with its description.
    B  ``tier-`` lists the ``.agents`` and the ``.claude`` tier skills.
    C  ``dis-``  lists ``dis-shown`` and not ``dis-hidden`` (``[skills] disabled``).
    D  ``dup-``  lists the ``.agents`` copy of a duplicated name and not the ``.claude`` one.
    E  ``fil-``  lists both valid skills and not the invalid ``Fil-Bad``; ``al`` narrows to
       ``fil-alpha``; the validation Warning is in the Notification Log (mark taken before start).
    """
    ws = tmp_path / "ws"
    home = tmp_path / "home"
    ws.mkdir()
    home.mkdir()
    write_skill(ws, ".agents/skills", "reviewer", "Reviews code for bugs")
    write_skill(ws, ".agents/skills", "tier-agents", "Agents tier skill")
    write_skill(ws, ".claude/skills", "tier-claude", "Claude tier skill")
    write_skill(ws, ".agents/skills", "dis-shown", "Shown skill")
    write_skill(ws, ".agents/skills", "dis-hidden", "Hidden skill")
    write_rustain_config(ws, disabled=["dis-hidden"])
    write_skill(ws, ".agents/skills", "dup-skill", "Agents tier dup")
    write_skill(ws, ".claude/skills", "dup-skill", "Claude tier dup")
    write_skill(ws, ".agents/skills", "fil-alpha", "Filter alpha")
    write_skill(ws, ".agents/skills", "fil-beta", "Filter beta")
    write_skill(ws, ".agents/skills", "Fil-Bad", "Invalid uppercase name")

    mark = NotificationMark(int(time.time()))  # the discovery notice is raised after start()
    tui = _start_tui_with_workspace(ws, isolated_home=home)
    try:
        # A — built-ins fill the first rows, so the filter must reach the skill.
        _filter(tui, "/rev", ("reviewer", "Reviews code for bugs"), label="reviewer and its description")
        _reset(tui, "/rev", "reviewer")
        # B — both tiers are scanned.
        _filter(tui, "/tier-", ("tier-agents", "tier-claude"), label="both tiers")
        _reset(tui, "/tier-", "tier-agents")
        # C — a disabled skill is hidden (anchored on its enabled neighbour in the same frame).
        _filter(tui, "/dis-", ("dis-shown",), ("dis-hidden",), label="enabled, not disabled")
        _reset(tui, "/dis-", "dis-shown")
        # D — the higher-priority tier wins a duplicated name.
        _filter(tui, "/dup-", ("Agents tier dup",), ("Claude tier dup",), label="the .agents copy")
        _reset(tui, "/dup-", "Agents tier dup")
        # E — an invalid skill is excluded; the filter narrows on the next keys.
        _filter(tui, "/fil-", ("fil-alpha", "fil-beta"), ("Fil-Bad",), label="valid, not invalid")
        _filter(tui, "al", ("fil-alpha",), ("fil-beta",), label="narrowed to fil-alpha")
        _reset(tui, "/fil-al", "fil-alpha")
        tui.wait_for_notification("failed validation", mark, "Warning")
    finally:
        tui.stop()
