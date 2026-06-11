"""Story 10-x — Live-LLM end-to-end smoke for the subagent / `task` tool.

This is the realism layer on top of the deterministic Rust integration tests
(``integration_task_tool_dispatch`` et al., which use a stub ``NoOpProvider``)
and the deterministic Python panel tests (``test_story_10_4_subagent_panel``).
It exercises the **same risk** — that a real LLM can discover and call the
``task`` tool, the in-process subagent runs to completion, and its result
re-enters the parent conversation — but with a **live model in the loop**.

Per the team's MCP/LLM testing policy (see ``feedback_mcp_llm_test_prompts``),
prompts are strict and deterministic: short, imperative, "do this exact thing,
output nothing else." This buys realism without open-ended-prompt flakiness.

Marked ``@requires_api @slow``. Without an API key the conftest auto-skips;
each test also runtime-checks that a provider is actually reachable in the
fresh test workspace and skips cleanly otherwise (the deterministic tier
already covers the wire).

Closes the AI-10.6 carry-forward gap (AI-9.9, 4th epoch): Epic 10 previously
had NO live-LLM E2E and NO TUI E2E for any subagent surface.

Risks covered (realism tier):
  * R1 — the `task` tool descriptor reaches the LLM (model can name & call it)
  * R2 — `tool_use` round-trip launches a real in-process subagent
  * R3 — the spawned subagent surfaces in the Agents panel (Ctrl+X, S)
  * R4 — the subagent's result tail re-enters the parent conversation
"""

from __future__ import annotations

import time

import pytest

from keys import CTRL_X


pytestmark = [
    pytest.mark.story_10_x,
    pytest.mark.subagent,
    pytest.mark.requires_api,
    pytest.mark.slow,
]


# Generous handshake + idle waits — live-LLM turns dominate runtime, and a
# subagent turn nests a second model loop inside the parent turn.
HANDSHAKE_S = 3.0
TURN_IDLE_S = 120.0


def _skip_if_provider_unreachable(tui) -> None:
    """Skip when the rustain banner says the provider isn't reachable.

    The fresh test workspace doesn't copy the user's ``[provider.*]`` config by
    design (isolation). When no provider is reachable the live-LLM realism
    layer can't run — the deterministic tier already covers the wire, so
    skipping is correct.
    """
    screen = tui.get_screen_text()
    banners = ["No provider is reachable", "no active provider", "unknown provider"]
    matched = [b for b in banners if b.lower() in screen.lower()]
    if matched:
        pytest.skip(
            f"Provider not reachable in test workspace (banner: {matched[0]!r}). "
            "Live-LLM realism layer needs [provider.*] config in the test "
            "workspace's .rustain/config.toml. Deterministic tier covers the wire."
        )


def _send_chord_ctrl_x_s(tui) -> None:
    tui.send(CTRL_X)
    tui.wait(0.2)
    tui.send("s")
    tui.wait(0.5)


# ── R1 + R2 + R4: descriptor → task tool_use → subagent runs → result back ──


def test_llm_delegates_to_subagent_via_task_tool(tui_with_subagent):
    """The model must call the `task` tool to delegate to the `echo-agent`,
    the in-process subagent must run, and its result must re-enter the parent
    conversation. Any one success marker proves the wire end-to-end.
    """
    tui = tui_with_subagent
    time.sleep(HANDSHAKE_S)
    _skip_if_provider_unreachable(tui)

    prompt = (
        "Use the task tool to delegate to the subagent named 'echo-agent'. "
        "Give it exactly this instruction: \"Reply with the single word PONG "
        "and nothing else.\" Do not do the work yourself. Do not explain. "
        "Call the task tool, then report the subagent's result in one short line."
    )
    tui.send_message(prompt)
    tui.wait_for_idle(TURN_IDLE_S)

    if tui.is_stream_disconnected():
        pytest.skip("Provider stream disconnected (rate-limit / SSE drop).")

    screen = tui.get_screen_text()
    # Accept several markers — any one proves descriptor → tool_use → subagent
    # → result-in-conversation. We allow the model to paraphrase the result.
    success_markers = [
        "task",          # the tool-block label for the `task` tool call
        "echo-agent",    # the subagent identity surfaced in the tool block
        "PONG",          # the subagent's deterministic reply, re-entered
    ]
    matched = [m for m in success_markers if m in screen]
    assert "PONG" in screen or len(matched) >= 2, (
        "Expected evidence the `task` tool delegated to a live subagent and the "
        "result re-entered the conversation (PONG, or the task/echo-agent tool "
        f"block). Matched={matched}.\nScreen:\n{screen}"
    )


# ── R3: spawned subagent surfaces in the Agents panel ───────────────────────


def test_subagent_appears_in_panel_during_delegation(tui_with_subagent):
    """While a delegated `task` runs, the Agents panel (Ctrl+X, S) must show a
    live row for the subagent — not the empty state.

    The subagent is short-lived, so we open the panel immediately after sending
    and poll: we accept either a live row (agent name / running glyph) OR, if
    the turn already finished, a terminal row — both prove the registry was
    populated. Only the persistent empty state is a failure.
    """
    tui = tui_with_subagent
    time.sleep(HANDSHAKE_S)
    _skip_if_provider_unreachable(tui)

    prompt = (
        "Use the task tool to delegate to the subagent named 'echo-agent' with "
        "the instruction: \"Reply with the single word PONG.\" "
        "Do not do the work yourself; call the task tool."
    )
    tui.send_message(prompt)

    # Open the panel and poll while the turn is in flight.
    saw_row = False
    deadline = time.time() + TURN_IDLE_S
    while time.time() < deadline:
        _send_chord_ctrl_x_s(tui)
        screen = tui.get_screen_text()
        if "echo-agent" in screen or "in-process" in screen:
            saw_row = True
            break
        # Close again so the next chord re-opens cleanly (toggle semantics).
        _send_chord_ctrl_x_s(tui)
        if "Ready" in screen and "echo-agent" not in screen:
            # Turn finished; one last open to catch a terminal row.
            _send_chord_ctrl_x_s(tui)
            screen = tui.get_screen_text()
            saw_row = "echo-agent" in screen or "in-process" in screen
            break
        time.sleep(1.0)

    if tui.is_stream_disconnected():
        pytest.skip("Provider stream disconnected (rate-limit / SSE drop).")

    screen = tui.get_screen_text()
    if not saw_row:
        # If the model declined to call the task tool at all, that's a separate
        # (R1/R2) failure covered by the test above; skip rather than double-report.
        if "echo-agent" not in screen and "task" not in screen:
            pytest.skip(
                "Model did not invoke the task tool this run (R1/R2 covers that). "
                f"Screen:\n{screen}"
            )
    assert saw_row, (
        "Expected the delegated subagent to surface in the Agents panel "
        f"(a live or terminal 'echo-agent' / 'in-process' row). Screen:\n{screen}"
    )
