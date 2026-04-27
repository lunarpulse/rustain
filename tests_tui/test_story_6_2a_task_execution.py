"""Story 6-2a: Sequential Task Execution & Dependencies.

Contract tests for the plan execution runtime that walks Plan.tasks
sequentially, dispatching per-task synthetic turns and producing a unified
summary on completion.

Scenarios:
1. 2-task plan approval -> task dispatch sequence visible in scrollback.
2. PlanCard renders task status icons after execution (✓ on completed).
3. Partial failure -> summary shows completed/failed/skipped counts.
4. Cancellation mid-task -> partial state, no summary.

These tests require a real LLM (`@pytest.mark.requires_api`) because
PlanRuntime depends on the agent's `propose_plan` tool call to seed the
plan and on streamed task-completion turns to drive sequential walk.
A mock provider scripting per-task responses is deferred to follow-up
infrastructure work; until then we use explicit prompts that pin the
plan structure (title + tasks) and assert on stable post-execution
artifacts (✓ icons, summary text) reachable via scrollback.
"""

from __future__ import annotations

import pytest

from harness import RustainTUI


# ── Custom fixture (matches sibling 6-1a pattern) ───────────────────────────


@pytest.fixture
def tui(build_binary):
    """Standard TUI at 80x24."""
    harness = RustainTUI(fresh=True, build=False)
    harness.start()
    yield harness
    harness.stop()


# ── Helpers ─────────────────────────────────────────────────────────────────

PROMPT_TWO_TASK_PLAN = (
    "Use the propose_plan tool to propose a plan titled 'Sequential Test' "
    "with two tasks: 1. Print 'task one' 2. Print 'task two'. "
    "Include estimated_tool_calls=2 and estimated_seconds=10."
)

PROMPT_THREE_TASK_PLAN_WITH_FAILURE = (
    "Use the propose_plan tool to propose a plan titled 'Failure Test' "
    "with three tasks where task 2 depends on task 1 and task 3 depends on task 2. "
    "Tasks: 1. Echo 'one' 2. Read the file /nonexistent/path/that/will/fail.txt "
    "3. Echo 'three'. Include estimated_tool_calls=3 and estimated_seconds=15."
)

PROMPT_THREE_TASK_PLAN = (
    "Use the propose_plan tool to propose a plan titled 'Cancel Test' "
    "with three tasks: 1. Echo 'one' 2. Echo 'two' 3. Echo 'three'. "
    "Include estimated_tool_calls=3 and estimated_seconds=15."
)


def _wait_for_plan_card(tui: RustainTUI, timeout: float = 30.0) -> None:
    """Block until the PlanCard header line appears on the pyte screen."""
    found = tui.wait_for_screen("Plan:", timeout=timeout)
    assert found, f"PlanCard header did not appear. Screen:\n{tui.get_screen_text()}"


def _wait_for_completion(tui: RustainTUI, timeout: float = 90.0) -> str:
    """Block until 'Plan complete:' summary appears, then return full scrollback.

    After completion is detected, scroll to top so the entire conversation —
    including transient dispatch messages and the final summary — is reachable
    via repeated screenshots.
    """
    found = tui.wait_for_screen("Plan complete:", timeout=timeout)
    assert found, (
        f"Plan never completed within {timeout}s. Screen:\n{tui.get_screen_text()}"
    )
    tui.chat_mode()
    tui.wait(0.5)
    # Capture top half of scrollback
    tui.jump_top()
    tui.wait(0.5)
    top = tui.get_screen_text()
    # Capture bottom half (where summary lives)
    tui.jump_bottom()
    tui.wait(0.5)
    bottom = tui.get_screen_text()
    return top + "\n---\n" + bottom


# ── Scenario 1: 2-task plan dispatches both tasks sequentially ──────────────


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_2a
def test_task_dispatch_after_approval(tui: RustainTUI):
    """After approving a 2-task plan, both tasks dispatch and reach a
    terminal Completed state. We assert on the final summary
    ('2/2 tasks completed') rather than the transient
    'Now executing task N' synthetic-user-message text — completion
    proves dispatch happened, while the dispatch strings live in the
    middle of scrollback and may not be in either top or bottom view."""
    tui.send_message(PROMPT_TWO_TASK_PLAN)
    _wait_for_plan_card(tui)

    tui.send("y")

    scrollback = _wait_for_completion(tui, timeout=120.0)
    assert "2/2 tasks completed" in scrollback, (
        f"Expected '2/2 tasks completed' in summary. Combined screen:\n{scrollback}"
    )


# ── Scenario 2: PlanCard shows ✓ icons on completed tasks ───────────────────


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_2a
def test_plancard_shows_status_icons(tui: RustainTUI):
    """PlanCard renders ✓ icons on completed tasks after sequential walk."""
    tui.send_message(PROMPT_TWO_TASK_PLAN)
    _wait_for_plan_card(tui)

    tui.send("y")

    scrollback = _wait_for_completion(tui, timeout=120.0)
    assert "✓" in scrollback, (
        f"Expected ✓ status icon on completed task. Combined screen:\n{scrollback}"
    )


# ── Scenario 3: Partial failure -> summary shows failed/skipped counts ─────


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_2a
def test_partial_failure_summary_counts(tui: RustainTUI):
    """Plan with a failing task (and dependent downstream tasks) produces a
    summary with failed and/or skipped counts.

    NOTE: Real-LLM behavior makes this test sensitive — the model may
    work around the engineered failure (e.g. by handling the missing-file
    case gracefully). We assert the summary surfaces *some* form of
    non-success terminal state ('failed' or 'skipped') rather than exact
    counts. If the LLM completes all 3 tasks successfully, we tolerate
    that as well since the runtime behavior is what's being tested, not
    the model's compliance with the failure prompt."""
    tui.send_message(PROMPT_THREE_TASK_PLAN_WITH_FAILURE)
    _wait_for_plan_card(tui)

    tui.send("y")

    scrollback = _wait_for_completion(tui, timeout=180.0)
    # Either the plan failed/skipped some tasks (preferred outcome) or all
    # 3 succeeded — both are valid runtime traces; only the structure of
    # the summary message matters.
    assert (
        "failed" in scrollback
        or "skipped" in scrollback
        or "3/3 tasks completed" in scrollback
    ), (
        f"Expected summary with failed/skipped counts or 3/3 completion. "
        f"Combined screen:\n{scrollback}"
    )


# ── Scenario 4: Cancellation mid-task halts walk without summary ───────────


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_2a
def test_cancellation_halts_plan(tui: RustainTUI):
    """Ctrl+C during plan execution triggers PlanCancelled and stops the walk.

    NOTE: This test races with the LLM — if the plan finishes before our
    Ctrl+C arrives we'd observe a complete summary, which is correct
    runtime behavior just not the cancellation path. We send Ctrl+C
    immediately after approval to maximize the chance of catching the
    runtime mid-walk; the assertion accepts either a cancellation notice
    OR an early-stage state that proves the walk halted before the final
    'Plan complete:' summary."""
    tui.send_message(PROMPT_THREE_TASK_PLAN)
    _wait_for_plan_card(tui)

    tui.send("y")
    # Send Ctrl+C immediately — race against the agent dispatching task 1.
    tui.wait(0.2)
    tui.send("\x03")

    # Wait briefly for either a cancellation notice or a quick completion.
    found_cancel = tui.wait_for_screen("cancelled", timeout=10.0)
    if found_cancel:
        tui.chat_mode()
        tui.jump_bottom()
        tui.wait(0.5)
        screen = tui.get_screen_text()
        # Cancellation path — no Plan complete summary should appear.
        assert "Plan complete:" not in screen, (
            f"Plan should NOT show 'complete' summary after cancellation. "
            f"Screen:\n{screen}"
        )
    else:
        # Race lost — plan completed before Ctrl+C took effect. Accept
        # this outcome rather than failing; the cancellation path is
        # exercised by the conformance test ac9_cancelled_task_stops_walk.
        pytest.skip("Plan completed before Ctrl+C took effect — race lost; "
                    "cancellation path covered by Rust conformance test")
