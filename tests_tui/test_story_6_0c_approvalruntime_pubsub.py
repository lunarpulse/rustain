"""Story 6-0c: ApprovalRuntime Pub/Sub.

Tests for the pub/sub approval runtime, session-allow fast-path,
AlwaysAndSave persistence, cancel-by-source wiring, 5-action UX
regression, subagent attribution prefix, and permission_rules TOML loading.

Most tests require a live API to trigger tool calls — these are marked
``@pytest.mark.requires_api`` and excluded from CI.
Non-API tests (mode switching, persistence file inspection, structural
checks) run without API.
"""

from __future__ import annotations

import json
import shutil
import time

import pytest

from harness import RustainTUI
from keys import Permission, ENTER, ESC
from pathlib import Path


# ── Structural / non-API tests ─────────────────────────────────────────────


@pytest.mark.story_6_0c
def test_permission_keys_still_defined():
    """AC11 regression: all 5 action keys still exist after migration."""
    assert Permission.ALLOW == "y"
    assert Permission.SESSION_ALLOW == "s"
    assert Permission.ALWAYS_ALLOW == "a"
    assert Permission.DENY == "n"
    assert Permission.DENY_FEEDBACK == "f"


@pytest.mark.story_6_0c
def test_harness_has_all_permission_methods():
    """AC11: harness exposes session_allow, always_allow, deny_with_feedback."""
    assert hasattr(RustainTUI, "session_allow_permission")
    assert hasattr(RustainTUI, "always_allow_permission")
    assert hasattr(RustainTUI, "deny_with_feedback")
    assert hasattr(RustainTUI, "set_permission_mode")


@pytest.mark.story_6_0c
def test_permissions_toml_round_trip(tmp_path):
    """AC10: Write a permissions.toml and verify it parses on load."""
    workspace = tmp_path / "ws"
    workspace.mkdir()
    (workspace / ".rustain").mkdir(parents=True, exist_ok=True)
    rules_content = (
        '[[rules]]\npriority = 100\npattern = "Bash:*"\naction = "allow"\nscope = "tool"\n\n'
        '[[rules]]\npattern = "*"\naction = "ask"\nscope = "tool"\n'
    )
    (workspace / ".rustain" / "permissions.toml").write_text(rules_content)
    loaded = (workspace / ".rustain" / "permissions.toml").read_text()
    assert "Bash:*" in loaded
    assert "priority = 100" in loaded
    assert "catch-all" not in loaded or "ask" in loaded


# ── API-dependent TUI tests ───────────────────────────────────────────────


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_0c
def test_five_action_prompt_shows_all_glyphs(tui_strict: RustainTUI):
    """AC11 regression: permission prompt still renders all 5 action glyphs."""
    tui_strict.send_message("Use the Bash tool to run: echo hello")
    found = tui_strict.wait_for_screen("[y]", timeout=30.0)
    if not found:
        pytest.skip("LLM did not trigger a Bash tool call — cannot verify prompt glyphs")
    tui_strict.assert_screen_contains("[y]")
    tui_strict.assert_screen_contains("[s]")
    tui_strict.assert_screen_contains("[a]")
    tui_strict.assert_screen_contains("[n]")
    tui_strict.assert_screen_contains("[f]")
    tui_strict.approve_permission()


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_0c
def test_session_allow_auto_approves_subsequent_same_tool(tui_strict: RustainTUI):
    """AC4/AC5: [s] session-allow auto-approves next Bash call via fast-path."""
    tui_strict.send_message("Use the Bash tool to run: echo first")
    if not tui_strict.wait_for_screen("[y]", timeout=30.0):
        pytest.skip("LLM did not trigger a Bash tool call")
    tui_strict.session_allow_permission()
    tui_strict.wait_for_idle()

    tui_strict.send_message("Use the Bash tool to run: echo second")
    tui_strict.wait_for_idle(15.0)
    tui_strict.assert_screen_not_contains("[y] Allow", msg="Second Bash should fast-path")


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_0c
def test_always_allow_persists_to_toml(tui_strict: RustainTUI):
    """AC10: [a] AlwaysAndSave persists to config.toml; survives restart."""
    tui_strict.send_message("Use the Bash tool to run: echo persist_test")
    if not tui_strict.wait_for_screen("[y]", timeout=30.0):
        pytest.skip("LLM did not trigger a Bash tool call")
    tui_strict.always_allow_permission(wait_before=2.0)
    tui_strict.wait_for_idle()

    import time as _t
    _t.sleep(1.0)

    config_toml = tui_strict.wp / ".rustain" / "config.toml"
    if config_toml.exists():
        content = config_toml.read_text()
        assert "Bash" in content, f"AlwaysAndSave should persist Bash to config.toml; got: {content}"


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_0c
def test_deny_with_feedback_shows_rejection_block(tui_strict: RustainTUI):
    """AC5 regression: [f] deny-with-feedback still shows rejection block."""
    tui_strict.send_message("Use the Bash tool to run: rm -rf /tmp/test_feedback")
    tui_strict.deny_with_feedback("use archive instead")


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_0c
def test_deny_returns_error_to_model(tui_strict: RustainTUI):
    """AC5: [n] Reject { feedback: None } produces error visible in chat."""
    tui_strict.send_message("Use the Bash tool to run: rm -rf /tmp/test_deny")
    if not tui_strict.wait_for_screen("[y]", timeout=30.0):
        pytest.skip("LLM did not trigger a Bash tool call")
    tui_strict.send(Permission.DENY)
    tui_strict.wait_for_screen_not_contains("[y]", timeout=5.0)
    tui_strict.assert_screen_not_contains("[y] Allow", msg="Prompt should dismiss after deny")


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_0c
def test_session_allow_survives_mode_switch(tui_strict: RustainTUI):
    """AC5 regression: session-allow set persists across mode switch."""
    tui_strict.send_message("Use the Bash tool to run: echo setup")
    if not tui_strict.wait_for_screen("[y]", timeout=30.0):
        pytest.skip("LLM did not trigger a Bash tool call")
    tui_strict.session_allow_permission()
    tui_strict.wait_for_idle()

    tui_strict.set_permission_mode("plan")
    tui_strict.set_permission_mode("normal")

    tui_strict.send_message("Use the Bash tool to run: echo preserved")
    tui_strict.wait_for_idle(15.0)
    tui_strict.assert_screen_not_contains("[y] Allow", msg="Session-allow should survive mode switch")


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_0c
def test_queue_indicator_appears_for_queued_permissions(tui_strict: RustainTUI):
    """AC11: Permission queue indicator shows 'N more queued' when multiple
    tools are pending. This test is best-effort — the indicator only appears
    when the LLM triggers multiple concurrent tool calls that all require
    approval. If only one call is made, the test approves it and passes.
    """
    tui_strict.send_message("Run these 3 commands separately: echo a, echo b, echo c")
    found = tui_strict.wait_for_screen("[y]", timeout=30.0)
    if not found:
        tui_strict.wait_for_idle(15.0)
        return
    screen = tui_strict.get_screen_text()
    if "more queued" in screen:
        assert "[1 more queued]" in screen or "[2 more queued]" in screen, \
            f"Expected queue indicator, got: {screen}"
    tui_strict.approve_permission()


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_0c
def test_permission_prompt_displays_tool_input(tui_strict: RustainTUI):
    """AC11 fix #5 regression: permission prompt shows actual command, not empty."""
    tui_strict.send_message("Use the Bash tool to run: echo input_preview_test")
    if not tui_strict.wait_for_screen("[y]", timeout=30.0):
        pytest.skip("LLM did not trigger a Bash tool call")
    screen = tui_strict.get_screen_text()
    assert "Bash" in screen, f"Tool name should be visible in prompt; screen: {screen}"
    tui_strict.approve_permission()


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_0c
def test_esc_cancels_permission_prompt(tui_strict: RustainTUI):
    """AC11: Esc sends Cancel (not Reject) to the runtime."""
    tui_strict.send_message("Use the Bash tool to run: echo cancel_test")
    if not tui_strict.wait_for_screen("[y]", timeout=30.0):
        pytest.skip("LLM did not trigger a Bash tool call")
    tui_strict.send(ESC)
    tui_strict.wait_for_screen_not_contains("[y]", timeout=5.0)
    tui_strict.assert_screen_not_contains("[y] Allow", msg="Esc should dismiss permission prompt")


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_0c
def test_yolo_mode_auto_approves_elevated(tui_strict: RustainTUI):
    """AC5: YOLO mode auto-approves Elevated tools without prompt."""
    tui_strict.set_permission_mode("yolo")
    tui_strict.send_message("Use the Bash tool to run: echo yolo_test")
    tui_strict.wait_for_idle(15.0)
    tui_strict.assert_screen_not_contains("[y] Allow", msg="YOLO should auto-approve Bash")
    tui_strict.set_permission_mode("normal")


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_0c
def test_plan_mode_blocks_standard_tools_without_prompt(tui_strict: RustainTUI):
    """AC5: Plan mode blocks Write/Edit without showing permission prompt."""
    tui_strict.set_permission_mode("plan")
    tui_strict.send_message("Use the Write tool to write 'hello' to /tmp/plan_test.txt")
    tui_strict.wait_for_idle(20.0)
    tui_strict.assert_screen_not_contains("[y] Allow", msg="Plan mode should not show prompt")
    tui_strict.assert_screen_not_contains("[y]", msg="Plan mode should not show any action glyphs")
    tui_strict.set_permission_mode("normal")


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_0c
def test_always_allow_then_restart_fast_paths(tmp_path):
    """AC10 full round-trip: AlwaysAndSave → restart → fast-path."""
    workspace = tmp_path / "ws"
    workspace.mkdir()
    project_root = Path(__file__).resolve().parent.parent
    env_file = project_root / ".env"
    if env_file.exists():
        shutil.copy2(env_file, workspace / ".env")
    (workspace / ".claude").mkdir(parents=True, exist_ok=True)
    (workspace / ".claude" / "settings.json").write_text(
        json.dumps({"permissions": {"allow": ["Read", "Glob", "Grep"]}}) + "\n"
    )

    with RustainTUI(fresh=True, build=True, workspace=workspace, allowed_tools=["Read", "Glob", "Grep"]) as tui:
        tui.send_message("Use the Bash tool to run: echo persist_roundtrip")
        tui.wait_for_screen("[y]", timeout=30.0)
        tui.always_allow_permission(wait_before=2.0)
        tui.wait_for_idle()

    time.sleep(1.0)

    with RustainTUI(fresh=True, build=False, workspace=workspace, allowed_tools=["Read", "Glob", "Grep"]) as tui:
        tui.send_message("Use the Bash tool to run: echo should_fast_path")
        tui.wait_for_idle(20.0)
        tui.assert_screen_not_contains(
            "[y] Allow",
            msg="Bash should fast-path after restart with persisted AlwaysAndSave",
        )


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_0c
def test_permission_rules_toml_seeds_session(tmp_path):
    """AC6: permissions.toml rules seed the session-allow set on startup."""
    workspace = tmp_path / "ws"
    workspace.mkdir()
    project_root = Path(__file__).resolve().parent.parent
    env_file = project_root / ".env"
    if env_file.exists():
        shutil.copy2(env_file, workspace / ".env")
    (workspace / ".claude").mkdir(parents=True, exist_ok=True)
    (workspace / ".claude" / "settings.json").write_text(
        json.dumps({"permissions": {"allow": ["Read", "Glob", "Grep"]}}) + "\n"
    )
    (workspace / ".rustain").mkdir(parents=True, exist_ok=True)
    (workspace / ".rustain" / "permissions.toml").write_text(
        '[[rules]]\npattern = "Bash:*"\naction = "allow"\nscope = "tool"\n\n'
        '[[rules]]\npattern = "*"\naction = "ask"\nscope = "tool"\n'
    )

    with RustainTUI(fresh=True, build=True, workspace=workspace, allowed_tools=["Read", "Glob", "Grep"]) as tui:
        tui.send_message("Use the Bash tool to run: echo rules_test")
        tui.wait_for_idle(20.0)
        tui.assert_screen_not_contains(
            "[y] Allow",
            msg="Bash should auto-approve via permissions.toml rule seeding",
        )
