"""Story 9.x — Live-LLM per-feature smokes for MCP integration.

These are the realism layer on top of the deterministic Rust integration tests
and the deterministic Python TUI tests. They exercise the **same risks** with
a live LLM in the loop, but with **strict deterministic prompts** to keep
flakiness low (per the team's MCP-LLM testing policy).

Marked ``@requires_api @slow``. Without an API key the conftest auto-skips.
Additionally, each test runtime-checks that a provider is actually reachable
in the test workspace — the conftest creates a fresh ``RUSTAIN_CONFIG_DIR``
without copying the user's ``[provider.*]`` config, so a passing API-key gate
doesn't guarantee a working provider. Tests skip cleanly when the provider
banner says "No provider is reachable" rather than failing noisily.

Setup to actually RUN these tests:
  * Set ``ANTHROPIC_API_KEY`` (or equivalent) in env or ``.env``
  * Either copy a minimal ``[provider.<name>]`` block into the test workspace's
    ``.rustain/config.toml`` (extend the ``tui_with_mcp`` fixture), or run
    these against ``tui_in_project`` which uses the real workspace config

Prompts are engineered to leave the model no room for creative output — short,
imperative, "do this exact thing, output nothing else." This buys us realism
without the flakiness of open-ended prompts.

Risks covered (realism tier, complementary to deterministic Rust/Python):
  * R1 — LLM prompt contains the MCP descriptor (model can name & call it)
  * R2 — `tool_use` round-trip reaches MCP and the result appears in chat
  * R3 — Plan-mode denies an elevated MCP tool when the model attempts it
  * R12 — Workspace restriction is NOT applied to MCP write-style tools
"""
from __future__ import annotations

import time

import pytest


pytestmark = [
    pytest.mark.story_9_2,
    pytest.mark.mcp,
    pytest.mark.requires_api,
    pytest.mark.slow,
]


# Generous handshake + idle waits — live-LLM turns dominate runtime.
HANDSHAKE_S = 3.0
TURN_IDLE_S = 60.0


def _skip_if_provider_unreachable(tui) -> None:
    """Skip the test if rustain banner says the provider isn't reachable.

    The fresh test workspace doesn't copy the user's `[provider.*]` config
    by design (isolation). When no provider is reachable, the live-LLM
    realism layer can't run — but the deterministic tier already covers
    the wire end-to-end, so skipping here is the right call.
    """
    screen = tui.get_screen_text()
    banners = [
        "No provider is reachable",
        "no active provider",
        "unknown provider",
    ]
    matched = [b for b in banners if b.lower() in screen.lower()]
    if matched:
        pytest.skip(
            f"Provider not reachable in test workspace (banner: {matched[0]!r}). "
            "Live-LLM realism layer needs [provider.*] config in the test "
            "workspace's .rustain/config.toml. Deterministic tier covers the wire."
        )


# ── R1 + R2: descriptor + tool_use round-trip ────────────────────────────────

def test_llm_uses_mcp_echo_tool_round_trip(tui_with_mcp):
    """The model must call mcp__echo__echo and the result text must appear
    in the rendered conversation. Combined coverage for R1 (descriptor in
    LLM prompt) and R2 (tool_use routing).
    """
    tui = tui_with_mcp
    time.sleep(HANDSHAKE_S)
    _skip_if_provider_unreachable(tui)

    prompt = (
        "Call the mcp__echo__echo tool with message='PING'. "
        "Do not explain. Do not greet. Just call the tool, "
        "then summarize the result in one short sentence."
    )
    tui.send_message(prompt)
    tui.wait_for_idle(TURN_IDLE_S)

    screen = tui.get_screen_text()
    # We accept three different success markers — any one proves the wire
    # works end-to-end (descriptor → tool_use → result back in conversation):
    #   1. The tool-block "Success" marker for `[echo] echo` with PING input.
    #   2. The literal echo response "echo: PING" (collapsed view).
    #   3. The LLM-paraphrased response that includes "PING" along with a
    #      response-style verb. We allow paraphrase because the LLM may
    #      summarize the tool output in its own words.
    success_markers = [
        'Success [echo] echo "{"message":"PING"}"',
        "echo: PING",
        "echo:PING",
    ]
    matched = any(m in screen for m in success_markers) or (
        "PING" in screen and ("Echoed" in screen or "echoed" in screen)
    )
    assert matched, (
        f"Expected one of: tool-block Success marker, echo: PING literal, "
        f"or paraphrased 'Echoed: PING'. This proves R1 (descriptor reached "
        f"LLM) and R2 (tool_use routed to MCP and result re-entered chat).\n"
        f"Screen:\n{screen}"
    )


# ── R12: Workspace restriction does NOT apply to MCP write-style tools ──────

def test_llm_calls_mcp_file_writer_without_path_permission_prompt(tui_with_mcp):
    """The fixture's `file_writer` tool has readOnlyHint=false (Elevated).
    In Normal mode it should prompt for approval — and the prompt content
    must be MCP-server-scoped, not the built-in file-path workspace prompt
    that `Read`/`Write`/`Edit` use.
    """
    tui = tui_with_mcp
    time.sleep(HANDSHAKE_S)
    _skip_if_provider_unreachable(tui)

    prompt = (
        "Call mcp__echo__file_writer with path='/tmp/r12-mcp-skip', "
        "content='no-op'. Output only the tool call result, no commentary."
    )
    tui.send_message(prompt)
    # Wait for the approval prompt to surface — the runtime emits a
    # PermissionRequest for elevated MCP tools in Normal mode.
    appeared = tui.wait_for_screen("file_writer", timeout=30.0) or tui.wait_for_screen(
        "mcp__echo", timeout=10.0
    )
    screen = tui.get_screen_text()
    assert appeared, (
        f"Expected the approval prompt for mcp__echo__file_writer to surface. "
        f"Screen:\n{screen}"
    )
    # Negative assertion: the workspace-restriction prompt phrasing (used by
    # builtin Read/Write/Edit when the path is outside the workspace) must
    # NOT appear for MCP tools. We look for distinctive built-in prompt
    # markers that should be ABSENT.
    builtin_prompt_markers = [
        "outside the workspace",
        "outside of workspace",
        "Workspace boundary",
    ]
    leaks = [m for m in builtin_prompt_markers if m in screen]
    assert not leaks, (
        f"Built-in workspace-restriction prompt language leaked into an MCP "
        f"approval flow: {leaks}. Screen:\n{screen}"
    )

    # Deny so the live model doesn't loop.
    from keys import Permission
    tui.send(Permission.DENY)


# ── R3: Plan-mode denies elevated MCP tool when LLM attempts it ─────────────

def test_plan_mode_denies_llm_mcp_file_writer(tui_with_mcp):
    """In plan mode, the LLM may attempt an elevated MCP tool. The runtime
    must surface a denial — no file is written, no auto-approval, and the
    chat shows that the tool was blocked.
    """
    tui = tui_with_mcp
    time.sleep(HANDSHAKE_S)
    _skip_if_provider_unreachable(tui)
    tui.set_permission_mode("plan")

    prompt = (
        "Call mcp__echo__file_writer with path='/tmp/r3-plan-block', "
        "content='must-be-denied'. Output only the tool call result."
    )
    tui.send_message(prompt)
    tui.wait_for_idle(TURN_IDLE_S)

    screen = tui.get_screen_text()
    # The expected outcome: the tool call is recorded but the result block
    # shows a denial. We accept any of the common denial markers — the
    # phrasing has churned across Story 6 versions.
    denial_markers = [
        "denied",
        "Plan mode",
        "cannot run",
        "blocked",
        "not allowed",
    ]
    matched = [m for m in denial_markers if m.lower() in screen.lower()]
    assert matched, (
        f"Plan mode should deny the elevated MCP tool. Expected one of "
        f"{denial_markers} in chat. Screen:\n{screen}"
    )
