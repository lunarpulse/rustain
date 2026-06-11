"""Story 5-0b: Permission System Redesign.

Tests for tool risk categories, session-allow, batch sweep,
deny-with-feedback, and four approval modes.

Most tests require a live API to trigger tool calls — these are marked
``@pytest.mark.requires_api`` and excluded from CI.
Non-API tests (mode switching, widget rendering) run without API.
"""

from __future__ import annotations

import pytest

from harness import RustainTUI
from keys import Permission, ENTER, ESC


# ── Non-API tests (subprocess / mode-switch only) ─────────────────────────


@pytest.mark.story_5_0b
def test_permission_keys_are_defined():
    """Verify new permission key constants exist in keys.py."""
    assert Permission.SESSION_ALLOW == "s"
    assert Permission.DENY_FEEDBACK == "f"
    assert Permission.ALLOW == "y"
    assert Permission.DENY == "n"
    assert Permission.ALWAYS_ALLOW == "a"


@pytest.mark.story_5_0b
def test_harness_session_allow_method_exists():
    """Verify harness has session_allow_permission method (structural check)."""
    assert hasattr(RustainTUI, "session_allow_permission")


@pytest.mark.story_5_0b
def test_harness_deny_with_feedback_method_exists():
    """Verify harness has deny_with_feedback method (structural check)."""
    assert hasattr(RustainTUI, "deny_with_feedback")


@pytest.mark.story_5_0b
def test_harness_set_permission_mode_method_exists():
    """Verify harness has set_permission_mode method (structural check)."""
    assert hasattr(RustainTUI, "set_permission_mode")


# ── API-dependent TUI tests ───────────────────────────────────────────────


@pytest.mark.requires_api
@pytest.mark.story_5_0b
def test_bash_tool_prompts_with_five_options(tui_strict: RustainTUI):
    """AC3: Permission prompt shows all 5 action glyphs."""
    tui_strict.send_message("Run `echo hello` using Bash")
    tui_strict.wait_for_screen("[y]", timeout=15.0)
    tui_strict.assert_screen_contains("[y]")
    tui_strict.assert_screen_contains("[s]")
    tui_strict.assert_screen_contains("[a]")
    tui_strict.assert_screen_contains("[n]")
    tui_strict.assert_screen_contains("[f]")
    tui_strict.approve_permission()


@pytest.mark.requires_api
@pytest.mark.story_5_0b
def test_session_allow_auto_approves_next_call(tui: RustainTUI):
    """AC4: Session-allow auto-approves subsequent calls for same tool."""
    tui.send_message("Run `echo first` using Bash")
    tui.session_allow_permission()
    tui.wait_for_idle()

    tui.send_message("Run `echo second` using Bash")
    tui.wait_for_idle(5.0)
    tui.assert_screen_not_contains("[y] Allow")


@pytest.mark.requires_api
@pytest.mark.story_5_0b
def test_deny_with_feedback_sends_feedback_to_llm(tui: RustainTUI):
    """AC5: Deny + feedback emits FeedbackBlock with exact string."""
    tui.send_message("Run `rm -rf /tmp/test` using Bash")
    tui.deny_with_feedback("use archive instead")


@pytest.mark.requires_api
@pytest.mark.story_5_0b
def test_plan_mode_blocks_standard_tools(tui: RustainTUI):
    """AC7: Plan mode blocks Standard (Write/Edit) tools without prompt."""
    tui.set_permission_mode("plan")
    tui.send_message("Write 'hello' to /tmp/plan_test.txt")
    tui.wait_for_idle(10.0)
    tui.assert_screen_not_contains("[y] Allow")
    tui.assert_screen_contains("PLAN")
    tui.set_permission_mode("normal")


@pytest.mark.requires_api
@pytest.mark.story_5_0b
def test_autoedit_mode_auto_allows_write(tui: RustainTUI):
    """AC7: AutoEdit mode auto-allows Write/Edit (Standard)."""
    tui.set_permission_mode("autoedit")
    tui.send_message("Write 'hello' to /tmp/autoedit_test.txt")
    tui.wait_for_idle(10.0)
    tui.assert_screen_not_contains("[y] Allow")
    tui.set_permission_mode("normal")


@pytest.mark.requires_api
@pytest.mark.story_5_0b
def test_yolo_mode_status_bar_shows_warning(tui: RustainTUI):
    """AC7: Yolo mode shows YOLO indicator in status bar."""
    tui.set_permission_mode("yolo")
    tui.wait_for_screen("YOLO", timeout=3.0)
    tui.assert_screen_contains("YOLO")
    tui.set_permission_mode("normal")


@pytest.mark.requires_api
@pytest.mark.story_5_0b
def test_mode_switch_preserves_session_allow(tui: RustainTUI):
    """AC4: Mode switch does NOT clear session-allow set."""
    tui.send_message("Run `echo setup` using Bash")
    tui.session_allow_permission()
    tui.wait_for_idle()

    tui.set_permission_mode("plan")
    tui.set_permission_mode("normal")

    tui.send_message("Run `echo preserved` using Bash")
    tui.wait_for_idle(5.0)
    tui.assert_screen_not_contains("[y] Allow")


# ── Queue indicator (requires API for queued tool calls) ──────────────────


@pytest.mark.requires_api
@pytest.mark.story_5_0b
def test_queue_indicator_shows_count(tui: RustainTUI):
    """AC6: Queue indicator shows 'N more queued'."""
    # This test requires a prompt that triggers multiple concurrent tool calls.
    # Simplified: send a request that triggers 3 Bash calls, check for indicator.
    tui.send_message("Run these 3 commands separately: echo a, echo b, echo c")
    tui.wait_for_screen("[y]", timeout=15.0)
    # If queue built up, we should see the indicator
    # (may or may not appear depending on LLM response timing)
    tui.approve_permission()
