"""Story 5-3: Custom Slash Commands.

TUI contract tests for non-API-driven user interactions:
 - custom command discovery from ``.claude/commands/`` (AC1, AC6)
 - subdirectory namespacing ``ns:name`` (AC4)
 - autocomplete membership and filter narrowing (AC11)
 - re-scan on ``/`` popup open (AC5)
 - built-in command dispatch unchanged (AC12 regression)

Full invocation tests (AC2 body injection, AC7 ``{{args}}`` interpolation,
AC8 ``@{path}`` file refs) exercise the outgoing API payload and are covered
by the Rust integration suite (``tests/custom_slash_command_invocation.rs``).
This file focuses on TUI mechanics observable without an API key.
"""

from __future__ import annotations

import time

import pytest

from harness import RustainTUI, composer_top, mode_segment
from keys import ESC, ENTER, BACKSPACE, CTRL_U
from fixtures.commands import write_custom_command


CHAR_DELAY = 0.015


def type_slowly(t: RustainTUI, text: str, delay: float = CHAR_DELAY) -> None:
    for c in text:
        t.send(c)
        time.sleep(delay)


def _start_tui_with_workspace(workspace_path, isolated_home) -> RustainTUI:
    """``isolated_home`` keeps the developer's user-level commands out of the popup."""
    tui = RustainTUI(fresh=True, build=False, workspace=workspace_path,
                     env_overrides={"HOME": str(isolated_home)})
    tui.start()
    return tui


def _popup(screen: str) -> str:
    """The rows above the composer box (the autocomplete popup and the chat), never the
    composer's own typed text."""
    lines = screen.split("\n")
    top = composer_top(lines)
    return "\n".join(lines[:top]) if top is not None else ""


def _composer_text(screen: str) -> str:
    """The composer box (its top border down to the screen's last row)."""
    lines = screen.split("\n")
    top = composer_top(lines)
    return "\n".join(lines[top:]) if top is not None else ""


def _expect_popup(t: RustainTUI, shown, hidden=(), label: str = "") -> str:
    """Wait for a popup frame that lists every ``shown`` string and none of ``hidden``
    (a positive and an anchored negative in the same frame)."""
    return t.wait_until(
        lambda s: all(x in _popup(s) for x in shown) and not any(x in _popup(s) for x in hidden),
        f"the popup listing {shown} without {hidden} ({label})",
    )


def _reset(t: RustainTUI, token: str, anchor: str) -> None:
    """Close the popup with ``Esc``, clear whatever it left in the composer with ``Ctrl+U`` and wait
    for the typed token to leave. Never sent with no popup open: ``Esc`` would move focus to Chat."""
    t.send(ESC)
    t.wait_until(lambda s: anchor not in _popup(s), f"the popup for {token} closed")
    t.send(CTRL_U)
    assert t.wait_for_screen_not_contains(token, timeout=5.0), t.get_screen_text()


@pytest.mark.story_5_3
def test_custom_command_appears_in_autocomplete(build_binary, tmp_path):
    """AC1/AC3/AC4/AC5/AC11/AC12: one workspace, one launch; every popup leg reads the rows above the composer.

    A  ``rev``       lists ``review`` with its frontmatter description.
    B  ``hello``     lists ``hello`` with the first body line as its description (no frontmatter).
    C  ``deploy``    lists ``deploy:staging`` and ``deploy-prod``; ``-`` narrows to ``deploy-prod``;
       Backspace re-filters; ``ci`` lists the depth-3 ``ci:github:actions`` without the deploys.
    D  ``emp``       lists the empty-body ``empty``.
    E  ``/refresh-`` lists ``refresh-gone``; after the file is replaced on disk the next ``/``
       open lists ``refresh-new`` and not ``refresh-gone`` (re-scan adds and removes).
    F  ``/mode plan`` still dispatches the built-in although ``mode.md`` shadows it (last: leaves PLAN).
    """
    ws = tmp_path / "ws"
    home = tmp_path / "home"
    ws.mkdir()
    home.mkdir()
    write_custom_command(ws, "review.md", body="Review the code.\n", description="Review code")
    write_custom_command(ws, "hello.md", body="Greet the user warmly\nMore details here.\n")
    write_custom_command(ws, "deploy/staging.md", body="Deploy to staging.\n", description="Deploy staging")
    write_custom_command(ws, "deploy-prod.md", body="Deploy prod.\n", description="Deploy prod")
    write_custom_command(ws, "ci/github/actions.md", body="Run CI.\n", description="CI GitHub Actions")
    write_custom_command(ws, "empty.md", body="")
    gone = write_custom_command(ws, "refresh-gone.md", body="Soon gone.\n", description="Refresh gone")
    write_custom_command(ws, "mode.md", body="Shadow attempt.\n", description="Shadow mode")

    tui = _start_tui_with_workspace(ws, home)
    try:
        # A
        type_slowly(tui, "/rev")
        _expect_popup(tui, ("review", "Review code"), label="custom command with its description")
        _reset(tui, "/rev", "Review code")
        # B
        type_slowly(tui, "/hello")
        _expect_popup(tui, ("hello", "Greet the user warmly"), label="first-line description")
        _reset(tui, "/hello", "Greet the user warmly")
        # C
        type_slowly(tui, "/deploy")
        _expect_popup(tui, ("deploy:staging", "deploy-prod"), label="namespaced and flat")
        type_slowly(tui, "-")
        _expect_popup(tui, ("deploy-prod",), ("deploy:staging",), label="narrowed by '-'")
        tui.send_keys(*([BACKSPACE] * 7), delay=0.05)
        type_slowly(tui, "ci")
        _expect_popup(tui, ("ci:github:actions",), ("deploy-prod", "deploy:staging"),
                      label="depth-3 namespace after the Backspace re-filter")
        _reset(tui, "/ci", "ci:github:actions")
        # D
        type_slowly(tui, "/emp")
        _expect_popup(tui, ("empty",), label="empty-body command")
        _reset(tui, "/emp", "empty")
        # E
        type_slowly(tui, "/refresh-")
        _expect_popup(tui, ("refresh-gone",), ("refresh-new",), label="before the re-scan")
        _reset(tui, "/refresh-", "refresh-gone")
        gone.unlink()
        write_custom_command(ws, "refresh-new.md", body="Brand new.\n", description="Refresh new")
        type_slowly(tui, "/refresh-")
        _expect_popup(tui, ("refresh-new",), ("refresh-gone",), label="after the re-scan")
        _reset(tui, "/refresh-", "refresh-new")
        # F
        assert mode_segment(tui.get_screen_text()) != "PLAN", "the TUI must not spawn in Plan mode"
        type_slowly(tui, "/mode plan")
        tui.wait_until(lambda s: "/mode plan" in _composer_text(s), "'/mode plan' typed into the composer")
        tui.send_keys(ENTER, ENTER)
        tui.wait_until(lambda s: mode_segment(s) == "PLAN", "the PLAN status segment")
    finally:
        tui.stop()
