"""Story 11 (Epic 11) TUI E2E — memory & context commands.

Closes the traceability-gate PARTIALs that were "logic unit-tested, TUI E2E
missing" (see `_bmad-output/test-artifacts/traceability-matrix.md`) by proving
the commands are reachable and correctly ROUTED from the real TUI input:

  * ``/context off | on`` flips the status-bar ``mem: off`` segment   → 11-4 AC7
  * ``/context show`` / ``/context bogus`` are intercepted (no turn)  → 11-4 AC6
  * ``/memory consolidate`` / ``/memory forget`` are intercepted      → 11-2a / 11-4a

These are event-loop interceptions (``InputAction::ExecuteCommand``) that run
BEFORE any turn dispatch, so they need NO live LLM — deterministic, CI-safe.

## Observability notes (why the assertions are shaped this way)
Two ground-truth constraints discovered while authoring these tests:

1. **Command SystemNotices do NOT render on an empty "Welcome" conversation.**
   `/context show`, the consolidate/forget notices, etc. execute but produce no
   visible text until the conversation has at least one message (which needs a
   model turn). So the deterministic signals available without an API key are:
     * the status-bar ``mem: off`` segment (toggled by `/context off|on`), and
     * the ABSENCE of a dispatched turn — a correctly-intercepted command never
       creates a ``You:`` bubble or a provider error. We assert that negative:
       the screen stays on the welcome state with no ``You:`` / error banner.
   The handlers' visible output (cards, notice wording) is covered at the Rust
   unit level (context_command.rs / forget_command.rs / event_loop.rs tests).

2. **`/memory` routing fix.** `/context` had an explicit ExecuteCommand branch in
   app.rs but `/memory` did not — so `/memory consolidate` fell through to the
   user-defined SubmitWithContext path and dispatched an EMPTY turn (provider 400
   "Input cannot be empty"). This was a real bug these E2E tests surfaced; it is
   fixed in app.rs (slash_memory_routes_to_execute_command_with_subverb), and the
   no-turn assertions below would regress if it reappeared.

The fixtures isolate the provider config (disable an inherited ``openrouter``
provider from the dev ``.env`` and register ``anthropic``) — like ``tui_with_mcp``
— so startup is clean. The default ``coding`` profile wires ``project-scoped``
memory.
"""

from __future__ import annotations

import json
import tempfile
import time
from datetime import date
from pathlib import Path

import pytest

from harness import RustainTUI
from keys import ENTER

pytestmark = pytest.mark.story_11

_ALLOWED = ["Read", "Glob", "Grep"]

# Markers of a dispatched turn / provider round-trip — a correctly intercepted
# slash command produces NONE of these.
_TURN_MARKERS = (
    "You:",
    "Input cannot be empty",
    "Stream disconnected",
    "invalid_request_error",
)

# Single source of truth for the status-bar segment that `/context off|on` toggles
# (Story 11-4 AC7). Kept as one constant so a wording change is updated in one
# place rather than scattered across assertions.
_MEM_OFF_INDICATOR = "mem: off"

# Status-bar toggles render within ~1-2s; 4s leaves CI margin without masking an
# event-loop regression behind a long hang (was 8s).
_STATUS_BAR_TIMEOUT = 4.0


def _clean_config(allowed_tools: list[str]) -> str:
    """config.toml that disables an inherited ``openrouter`` provider (the dev
    ``.env`` points the default model at it) and registers ``anthropic`` from
    env-var auth, so the spawned TUI starts without provider-misconfig banners.
    Mirrors the provider block in conftest.py ``tui_with_mcp``.
    """
    return (
        "[permissions]\n"
        f"always_tools = {json.dumps(allowed_tools)}\n"
        "\n"
        "[provider.openrouter]\n"
        "enabled = false\n"
        'provider_id = "openrouter"\n'
        'model_id = "unused"\n'
        'api_key_env = "OPENROUTER_API_KEY"\n'
        "\n"
        "[provider.anthropic]\n"
        'provider_id = "anthropic"\n'
        'model_id = "glm-4.7"\n'
        'api_key_env = "ANTHROPIC_AUTH_TOKEN"\n'
        "enabled = true\n"
        'kind = "anthropic"\n'
    )


def _start_memory_tui(seed_daily: bool) -> tuple[tempfile.TemporaryDirectory, RustainTUI]:
    tmp = tempfile.TemporaryDirectory(prefix="rustain_mem11_")
    ws = Path(tmp.name)
    rustain_dir = ws / ".rustain"
    rustain_dir.mkdir(parents=True, exist_ok=True)
    (rustain_dir / "config.toml").write_text(_clean_config(_ALLOWED))

    if seed_daily:
        # Daily-log format (daily_log_memory.rs render_entry): `# DATE` H1 then
        # `## HH:MM:SS — summary` entries. Path = {workspace}/.rustain/memory/
        # {YYYY-MM-DD}.md (build_memory → workspace_path). Seeded BEFORE start so
        # DailyLogMemory loads it → memory.recent() is non-empty.
        memory_dir = rustain_dir / "memory"
        memory_dir.mkdir(parents=True, exist_ok=True)
        today = date.today().isoformat()
        (memory_dir / f"{today}.md").write_text(
            f"# {today}\n\n## 10:00:00 — reviewed the auth module refactor\n\n",
            encoding="utf-8",
        )

    harness = RustainTUI(fresh=True, build=False, workspace=ws, allowed_tools=_ALLOWED)
    harness.start()
    return tmp, harness


@pytest.fixture
def tui_memory(build_binary):
    """Clean-startup TUI with project-scoped memory and an empty workspace."""
    tmp, harness = _start_memory_tui(seed_daily=False)
    try:
        yield harness
    finally:
        harness.stop()
        tmp.cleanup()


@pytest.fixture
def tui_memory_seeded(build_binary):
    """Clean-startup TUI with TODAY's daily-log pre-seeded (non-empty recall)."""
    tmp, harness = _start_memory_tui(seed_daily=True)
    try:
        yield harness
    finally:
        harness.stop()
        tmp.cleanup()


def _run_command(tui: RustainTUI, command: str) -> None:
    """Type a slash command and submit it with a single Enter.

    Typed char-by-char (raw-mode TUIs drop burst input). A single Enter submits;
    the slash-command autocomplete does not capture it for these built-in
    commands. (Do NOT press Esc first — with no popup open, Esc switches to Chat
    focus and the command never submits.)
    """
    for ch in command:
        tui.send(ch)
        time.sleep(0.02)
    time.sleep(0.3)
    tui.send(ENTER)
    time.sleep(0.6)


def _assert_no_turn_dispatched(tui: RustainTUI) -> None:
    """A correctly-intercepted slash command creates no turn: the screen stays
    on the welcome state with no ``You:`` bubble and no provider error."""
    screen = tui.get_screen_text()
    for marker in _TURN_MARKERS:
        assert marker not in screen, (
            f"Command should be intercepted (no turn), but found {marker!r} — "
            f"it was dispatched as a message.\nScreen:\n{screen}"
        )


# ── /context off | on  + status-bar `mem: off`  (Story 11-4 AC7) ──────────────

class TestContextToggle:
    def test_off_shows_mem_off_indicator_then_on_clears_it(self, tui_memory: RustainTUI):
        """`/context off` surfaces the `mem: off` status-bar segment; `/context on`
        clears it again (round-trip). This is the user-visible AC7 toggle."""
        tui = tui_memory
        _run_command(tui, "/context off")
        assert tui.wait_for_screen(_MEM_OFF_INDICATOR, timeout=_STATUS_BAR_TIMEOUT), (
            f"Expected `{_MEM_OFF_INDICATOR}` status-bar segment after `/context off`.\n"
            f"Screen:\n{tui.get_screen_text()}"
        )
        _assert_no_turn_dispatched(tui)

        _run_command(tui, "/context on")
        assert tui.wait_for_screen_not_contains(_MEM_OFF_INDICATOR, timeout=_STATUS_BAR_TIMEOUT), (
            f"Expected `{_MEM_OFF_INDICATOR}` to clear after `/context on`.\n"
            f"Screen:\n{tui.get_screen_text()}"
        )


# ── /context show | bogus  (Story 11-4 AC6) ───────────────────────────────────

class TestContextShow:
    def test_show_is_intercepted_not_dispatched(self, tui_memory: RustainTUI):
        """`/context show` is handled in-loop — it must NOT become a chat turn."""
        _run_command(tui_memory, "/context show")
        _assert_no_turn_dispatched(tui_memory)
        # Still idle and ready (command consumed, no streaming turn started).
        assert tui_memory.wait_for_screen("Ready", timeout=5.0)

    def test_unknown_subcommand_is_intercepted(self, tui_memory: RustainTUI):
        """`/context bogus` is rejected in-loop (warning notice), not dispatched."""
        _run_command(tui_memory, "/context bogus")
        _assert_no_turn_dispatched(tui_memory)


# ── /memory consolidate  (Story 11-2a)  — routing + interception ──────────────

class TestMemoryConsolidate:
    def test_empty_history_is_intercepted(self, tui_memory: RustainTUI):
        """`/memory consolidate` on an empty workspace is intercepted (graceful
        notice), NOT dispatched as a turn — regression guard for the app.rs
        `/memory` routing fix."""
        _run_command(tui_memory, "/memory consolidate")
        _assert_no_turn_dispatched(tui_memory)
        assert tui_memory.wait_for_screen("Ready", timeout=5.0)

    def test_with_activity_is_intercepted(self, tui_memory_seeded: RustainTUI):
        """With a seeded daily-log entry, `/memory consolidate` is still
        intercepted by the event loop (it spawns the review sub-turn in the
        background rather than submitting the command text as a turn)."""
        _run_command(tui_memory_seeded, "/memory consolidate")
        _assert_no_turn_dispatched(tui_memory_seeded)


# ── /memory forget  (Story 11-4a AC-R0) — routing + interception ──────────────

class TestMemoryForget:
    def test_forget_is_intercepted(self, tui_memory: RustainTUI):
        """`/memory forget <text>` is intercepted (no derived index in the default
        build → no-match notice), NOT dispatched as a turn."""
        _run_command(tui_memory, "/memory forget zzznonexistentmemory")
        _assert_no_turn_dispatched(tui_memory)
        assert tui_memory.wait_for_screen("Ready", timeout=5.0)
