"""Story 11 (Epic 11) TUI E2E — memory & context commands.

Closes the traceability-gate PARTIALs that were "logic unit-tested, TUI E2E
missing" (see `_bmad-output/test-artifacts/traceability-matrix.md`) by proving
the commands are reachable and correctly ROUTED from the real TUI input:

  * ``/context off | on`` flips the status-bar ``mem: off`` segment   → 11-4 AC7
  * ``/context bogus`` is rejected in-loop (Warning notice)           → 11-4 AC6
  * ``/memory consolidate`` / ``/memory forget`` are intercepted      → 11-2a / 11-4a

These are event-loop interceptions (``InputAction::ExecuteCommand``) that run
BEFORE any turn dispatch, so they need NO live LLM — deterministic, CI-safe.

## Observability notes (why the assertions are shaped this way)
Since Story 21.1 every ``SystemNotice`` is a Notification Log record
(``src/infrastructure/runtime/system_notice.rs``), whether or not a toast paints on the
empty "Welcome" conversation. The handlers raise severity-specific notices that carry the
user's own token, so each command is told apart by ``wait_for_notification`` (severity,
typed token, and a mark taken before the first command), never by the absence of a turn:
  * ``/context bogus``       → Warning naming ``bogus``  (``context_command.rs``)
  * ``/memory forget <q>``   → Info naming ``<q>``       (``forget_command.rs``)
  * ``/memory consolidate``  → Info naming ``consolidate`` (empty history) or ``Reviewing``
    (seeded daily log; ``transparency_bridge.rs``)
The ``mem: off`` segment is read from the status row. Notice wording beyond those tokens is
covered at the Rust unit level (context_command.rs / forget_command.rs / event_loop.rs tests).

The ``/memory`` routing fix is the reason these two survivors exist: ``/context`` had an
explicit ExecuteCommand branch in app.rs but ``/memory`` did not, so ``/memory consolidate``
fell through to the user-defined SubmitWithContext path and dispatched an EMPTY turn (provider
400 "Input cannot be empty"); it is fixed in app.rs
(slash_memory_routes_to_execute_command_with_subverb).

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

from harness import RustainTUI, composer_top, status_row
from keys import ENTER

pytestmark = pytest.mark.story_11

_ALLOWED = ["Read", "Glob", "Grep"]

# Single source of truth for the status-bar segment that `/context off|on` toggles
# (Story 11-4 AC7). Kept as one constant so a wording change is updated in one
# place rather than scattered across assertions.
_MEM_OFF_INDICATOR = "mem: off"


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


def _composer_text(screen: str) -> str:
    """The composer box (its top border down to the screen's last row)."""
    lines = screen.split("\n")
    top = composer_top(lines)
    return "\n".join(lines[top:]) if top is not None else ""


def _run_command(tui: RustainTUI, command: str) -> None:
    """Type a slash command, wait until the composer holds it, and submit it with a single Enter.

    Typed char-by-char (raw-mode TUIs drop burst input). A single Enter submits;
    the slash-command autocomplete does not capture it for these built-in
    commands. (Do NOT press Esc first — with no popup open, Esc switches to Chat
    focus and the command never submits.)
    """
    for ch in command:
        tui.send(ch)
        time.sleep(0.02)
    tui.wait_until(lambda s: command in _composer_text(s), f"{command!r} typed into the composer")
    tui.send(ENTER)


# ── /context off | on, /context bogus, /memory forget | consolidate ───────────

def test_memory_commands_are_intercepted_on_an_empty_workspace(tui_memory: RustainTUI):
    """Each command reaches its own handler, told apart by its notice's severity and the token
    the user typed (the Notification Log is the freshness authority; one mark for the run).

    `/context off` surfaces the `mem: off` status-bar segment and `/context on` clears it again
    (Story 11-4 AC7); `/context bogus` raises a Warning naming `bogus` (11-4 AC6); `/memory forget
    <q>` an Info naming `<q>` (no derived index in the default build → no-match notice, 11-4a);
    `/memory consolidate` on an empty workspace an Info naming `consolidate` (11-2a). A command
    that missed the ExecuteCommand allowlist would raise none of these records.
    """
    tui = tui_memory
    assert _MEM_OFF_INDICATOR not in status_row(tui.get_screen_text())
    mark = tui.notification_mark()

    _run_command(tui, "/context off")
    tui.wait_until(lambda s: _MEM_OFF_INDICATOR in status_row(s),
                   f"the `{_MEM_OFF_INDICATOR}` status-bar segment after `/context off`")
    _run_command(tui, "/context on")
    tui.wait_until(lambda s: _MEM_OFF_INDICATOR not in status_row(s),
                   f"`{_MEM_OFF_INDICATOR}` cleared after `/context on`")

    _run_command(tui, "/context bogus")
    tui.wait_for_notification("bogus", mark, "Warning")

    _run_command(tui, "/memory forget zzznonexistentmemory")
    tui.wait_for_notification("zzznonexistentmemory", mark, "Info")

    _run_command(tui, "/memory consolidate")
    tui.wait_for_notification("consolidate", mark, "Info")


def test_consolidate_with_activity_is_intercepted(tui_memory_seeded: RustainTUI):
    """With a seeded daily-log entry (written before start), `/memory consolidate` is intercepted
    by the event loop and takes the review branch: an Info notice naming `Reviewing`."""
    tui = tui_memory_seeded
    mark = tui.notification_mark()
    _run_command(tui, "/memory consolidate")
    tui.wait_for_notification("Reviewing", mark, "Info")
