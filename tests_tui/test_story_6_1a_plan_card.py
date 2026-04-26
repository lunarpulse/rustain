"""Story 6-1a: Inline Plan Card.

Contract tests for the inline PlanCard widget rendered by the TUI when
the agent calls the propose_plan tool.

Scenarios:
1. propose_plan -> card renders with double border, numbered tasks,
   effort line, and [y][e][n] footer.
2. [y] approve -> [executing <ts>] status, next turn auto-fires.
3. [n] reject  -> [rejected <ts>] status, "Plan rejected" notice,
   synthetic user message visible in scrollback.
4. [e] edit    -> mocked $EDITOR modifies title -> card re-presents
   with updated title -> [y] approves.
5. YOLO mode   -> no pending card, auto-approved notice,
   plan visible in scrollback.

Snapshot tests capture the inline card rendering at 80x24 (double
border) and 60x16 (plain border) terminal sizes.
"""

from __future__ import annotations

import os
import stat

import pytest

from harness import RustainTUI
from keys import ENTER, ESC


# ── Custom Fixtures ─────────────────────────────────────────────────────────


@pytest.fixture
def tui_with_mock_editor(build_binary, tmp_path):
    """TUI with EDITOR mocked to a script that modifies the plan title."""
    editor_script = tmp_path / "mock_editor.sh"
    editor_script.write_text(
        "#!/bin/bash\n"
        "sed -i 's/^title = \".*\"/title = \"Edited Plan Title\"/' \"$1\"\n"
    )
    editor_script.chmod(editor_script.stat().st_mode | stat.S_IEXEC)

    old_editor = os.environ.get("EDITOR")
    os.environ["EDITOR"] = str(editor_script)
    try:
        harness = RustainTUI(fresh=True, build=False)
        harness.start()
        yield harness
        harness.stop()
    finally:
        if old_editor is not None:
            os.environ["EDITOR"] = old_editor
        else:
            os.environ.pop("EDITOR", None)


def _make_tui_at_size(rows, cols):
    """Factory that yields a RustainTUI at the given terminal dimensions."""
    import harness as h

    orig_rows, orig_cols = h.TERM_ROWS, h.TERM_COLS
    h.TERM_ROWS, h.TERM_COLS = rows, cols
    try:
        tui = RustainTUI(fresh=True, build=False)
        tui.start()
        yield tui
        tui.stop()
    finally:
        h.TERM_ROWS, h.TERM_COLS = orig_rows, orig_cols


@pytest.fixture
def tui_80x24(build_binary):
    """TUI at 80x24 for snapshot testing (double border, >=64 cols)."""
    yield from _make_tui_at_size(24, 80)


@pytest.fixture
def tui_60x16(build_binary):
    """TUI at 60x16 for snapshot testing (plain border, <64 cols)."""
    yield from _make_tui_at_size(16, 60)


# ── Helpers ─────────────────────────────────────────────────────────────────

PROMPT_PLAN = (
    "Use the propose_plan tool to propose a plan titled 'Test Plan' "
    "with two tasks: 1. Read the codebase 2. Write tests. "
    "Include estimated_tool_calls=3 and estimated_seconds=20."
)


def _wait_for_plan_card(tui: RustainTUI, timeout: float = 30.0) -> None:
    """Block until the PlanCard header line appears on the pyte screen."""
    found = tui.wait_for_screen("Plan:", timeout=timeout)
    assert found, f"PlanCard header did not appear. Screen:\n{tui.get_screen_text()}"


# ── Scenario 1: propose_plan renders inline card ────────────────────────────


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_1a
def test_propose_plan_renders_inline_card(tui: RustainTUI):
    """AC1/AC5: Agent calls propose_plan -> PlanCard renders with double
    border, numbered tasks, effort line, and [y][e][n] footer."""
    tui.send_message(PROMPT_PLAN)
    _wait_for_plan_card(tui)
    screen = tui.get_screen_text()

    assert "╔" in screen or "║" in screen, (
        f"Expected double-border characters at default width. Screen:\n{screen}"
    )

    assert "Plan:" in screen, f"Expected 'Plan:' header. Screen:\n{screen}"
    assert "1." in screen, f"Expected numbered task '1.'. Screen:\n{screen}"
    assert "2." in screen, f"Expected numbered task '2.'. Screen:\n{screen}"
    assert "Estimated" in screen or "tool calls" in screen, (
        f"Expected effort estimation line. Screen:\n{screen}"
    )

    assert "[y]" in screen, f"Expected '[y]' action key. Screen:\n{screen}"
    assert "[e]" in screen, f"Expected '[e]' action key. Screen:\n{screen}"
    assert "[n]" in screen, f"Expected '[n]' action key. Screen:\n{screen}"
    assert "Approve" in screen
    assert "Edit" in screen
    assert "Reject" in screen


# ── Scenario 2: [y] approve -> executing status, auto-fire ──────────────────


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_1a
def test_approve_plan_card(tui: RustainTUI):
    """AC7: Press [y] -> card status footer updates to [executing <ts>],
    next turn auto-fires (agent begins execution)."""
    tui.send_message(PROMPT_PLAN)
    _wait_for_plan_card(tui)

    tui.send("y")

    found = tui.wait_for_screen("[approved", timeout=10.0)
    assert found, (
        f"Expected '[approved' status after approve. Screen:\n{tui.get_screen_text()}"
    )

    tui.wait(1.0)
    screen = tui.get_screen_text()
    assert "[y] Approve  [e] Edit  [n] Reject" not in screen, (
        f"Action footer should be gone after approve. Screen:\n{screen}"
    )

    tui.wait_for_idle()


# ── Scenario 3: [n] reject -> rejected status, notice, synthetic msg ───────


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_1a
def test_reject_plan_card(tui: RustainTUI):
    """AC7: Press [n] -> [rejected <ts>] status footer, 'Plan rejected'
    notice, synthetic user message visible in scrollback."""
    tui.send_message(PROMPT_PLAN)
    _wait_for_plan_card(tui)

    tui.send("n")

    found = tui.wait_for_screen("[rejected", timeout=10.0)
    assert found, (
        f"Expected '[rejected' status after reject. Screen:\n{tui.get_screen_text()}"
    )

    found = tui.wait_for_screen("Plan rejected", timeout=5.0)
    assert found, (
        f"Expected 'Plan rejected' notice. Screen:\n{tui.get_screen_text()}"
    )

    tui.chat_mode()
    tui.jump_top()
    tui.wait(0.5)
    screen = tui.get_screen_text()
    assert "revis" in screen.lower() or "rejected" in screen.lower(), (
        f"Expected synthetic rejection message in scrollback. Screen:\n{screen}"
    )


# ── Scenario 4: [e] edit -> mock editor modifies title -> re-present ───────


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_1a
def test_edit_plan_card(tui_with_mock_editor: RustainTUI):
    """AC7: Press [e] -> editor opens (mock $EDITOR modifies title);
    on exit, card re-presents with updated title; press [y] to approve."""
    tui = tui_with_mock_editor
    tui.send_message(PROMPT_PLAN)
    _wait_for_plan_card(tui)

    tui.send("e")

    found = tui.wait_for_screen("Edited Plan Title", timeout=15.0)
    assert found, (
        f"Expected updated title 'Edited Plan Title' after edit. "
        f"Screen:\n{tui.get_screen_text()}"
    )

    screen = tui.get_screen_text()
    assert "[y]" in screen, (
        f"Card should still be pending after edit. Screen:\n{screen}"
    )

    tui.send("y")
    found = tui.wait_for_screen("[approved", timeout=10.0)
    assert found, (
        f"Expected '[approved' after approving edited plan. "
        f"Screen:\n{tui.get_screen_text()}"
    )


# ── Scenario 5: YOLO mode -> auto-approved, no pending card ────────────────


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_1a
def test_yolo_auto_approve_plan(tui: RustainTUI):
    """AC8: YOLO mode -> propose_plan produces no pending card; a single
    info notice with 'auto-approved' appears; plan is visible in scrollback."""
    tui.set_permission_mode("yolo")

    tui.send_message(PROMPT_PLAN)

    found = tui.wait_for_screen("auto-approved", timeout=30.0)
    assert found, (
        f"Expected 'auto-approved' notice in YOLO mode. Screen:\n{tui.get_screen_text()}"
    )

    screen = tui.get_screen_text()
    assert "[y] Approve" not in screen, (
        f"Should NOT show pending card footer in YOLO mode. Screen:\n{screen}"
    )

    tui.chat_mode()
    tui.wait(1.0)
    screen = tui.get_screen_text()
    assert "Plan:" in screen, (
        f"Expected plan card visible in scrollback. Screen:\n{screen}"
    )


# ── Snapshot: 80x24 (double border) ─────────────────────────────────────────


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_1a
def test_snapshot_plan_card_80x24(tui_80x24: RustainTUI):
    """Snapshot: inline PlanCard at 80x24 renders with double border."""
    tui = tui_80x24
    tui.send_message(PROMPT_PLAN)
    _wait_for_plan_card(tui, timeout=30.0)
    screen = tui.get_screen_text()

    assert any(c in screen for c in ("╔", "╗", "╚", "╝")), (
        f"Expected double border at 80 cols. Screen:\n{screen}"
    )
    assert "Plan:" in screen
    assert "[y]" in screen
    assert "[e]" in screen
    assert "[n]" in screen


# ── Snapshot: 60x16 (plain border) ──────────────────────────────────────────


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_1a
def test_snapshot_plan_card_60x16(tui_60x16: RustainTUI):
    """Snapshot: inline PlanCard at 60x16 renders with plain border."""
    tui = tui_60x16
    tui.send_message(PROMPT_PLAN)
    _wait_for_plan_card(tui, timeout=30.0)
    screen = tui.get_screen_text()

    assert any(c in screen for c in ("┌", "┐", "└", "┘")), (
        f"Expected plain border at 60 cols. Screen:\n{screen}"
    )
    assert not any(c in screen for c in ("╔", "╗", "╚", "╝")), (
        f"Double border should NOT appear at 60 cols. Screen:\n{screen}"
    )
    assert "Plan:" in screen
    assert "[y]" in screen
