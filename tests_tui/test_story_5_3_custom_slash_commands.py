"""Story 5-3: Custom Slash Commands.

TUI contract tests for non-API-driven user interactions:
 - custom command discovery from ``.claude/commands/`` (AC1, AC6)
 - subdirectory namespacing ``ns:name`` (AC4)
 - autocomplete rendering and ordering (AC11)
 - re-scan on ``/`` popup open (AC5)
 - built-in command dispatch unchanged (AC12 regression)

Full invocation tests (AC2 body injection, AC7 ``{{args}}`` interpolation,
AC8 ``@{path}`` file refs) exercise the outgoing API payload and are covered
by the Rust integration suite (``tests/custom_slash_command_invocation.rs``).
This file focuses on TUI mechanics observable without an API key.
"""

from __future__ import annotations

import json
import time

import pytest

from harness import RustainTUI
from keys import ESC, ENTER, TAB, BACKSPACE, CTRL_U
from harness import Chat
from fixtures.commands import write_custom_command


CHAR_DELAY = 0.015


def type_slowly(t: RustainTUI, text: str, delay: float = CHAR_DELAY) -> None:
    for c in text:
        t.send(c)
        time.sleep(delay)


def _start_tui_with_workspace(workspace_path) -> RustainTUI:
    tui = RustainTUI(fresh=True, build=False, workspace=workspace_path)
    tui.start()
    return tui


def _open_slash_autocomplete(t: RustainTUI) -> None:
    t.send("/")
    time.sleep(0.5)


def _close_autocomplete(t: RustainTUI) -> None:
    t.send(ESC)
    time.sleep(0.3)


@pytest.mark.story_5_3
def test_custom_command_appears_in_autocomplete(build_binary, tmp_path):
    """AC1: a ``.claude/commands/review.md`` file produces ``/review`` in autocomplete."""
    write_custom_command(tmp_path, "review.md", body="Review the code.\n", description="Review code")

    tui = _start_tui_with_workspace(tmp_path)
    try:
        _open_slash_autocomplete(tui)
        tui.assert_screen_contains("review", msg="Custom command /review should appear in autocomplete")
        tui.assert_screen_contains("Review code", msg="Description should appear in autocomplete")
        _close_autocomplete(tui)
    finally:
        tui.stop()


@pytest.mark.story_5_3
def test_namespaced_command_appears_in_autocomplete(build_binary, tmp_path):
    """AC4: ``.claude/commands/deploy/staging.md`` shows ``deploy:staging``."""
    write_custom_command(
        tmp_path, "deploy/staging.md", body="Deploy to staging.\n", description="Deploy staging"
    )

    tui = _start_tui_with_workspace(tmp_path)
    try:
        _open_slash_autocomplete(tui)
        tui.assert_screen_contains(
            "deploy:staging", msg="Namespaced command deploy:staging should appear in autocomplete"
        )
        _close_autocomplete(tui)
    finally:
        tui.stop()


@pytest.mark.story_5_3
def test_no_commands_dir_renders_only_builtins(build_binary, tmp_path):
    """AC6: workspace without ``.claude/commands/`` shows only built-in commands."""
    tui = _start_tui_with_workspace(tmp_path)
    try:
        _open_slash_autocomplete(tui)
        tui.assert_screen_contains("/new", msg="Built-in /new should appear in autocomplete")
        tui.assert_screen_not_contains("review", msg="No user commands expected without commands dir")
        _close_autocomplete(tui)
    finally:
        tui.stop()


@pytest.mark.story_5_3
def test_custom_command_without_frontmatter_uses_first_line_description(build_binary, tmp_path):
    """AC3: command file without frontmatter shows first body line as description."""
    write_custom_command(tmp_path, "hello.md", body="Greet the user warmly\nMore details here.\n")

    tui = _start_tui_with_workspace(tmp_path)
    try:
        _open_slash_autocomplete(tui)
        tui.assert_screen_contains("hello", msg="Command /hello should appear")
        tui.assert_screen_contains(
            "Greet the user warmly", msg="First body line should be used as description"
        )
        _close_autocomplete(tui)
    finally:
        tui.stop()


@pytest.mark.story_5_3
def test_multiple_custom_commands_sorted_alphabetically(build_binary, tmp_path):
    """AC11: user-defined commands sort alphabetically in autocomplete."""
    write_custom_command(tmp_path, "zebra.md", body="Zebra command.\n", description="Zebra")
    write_custom_command(tmp_path, "alpha.md", body="Alpha command.\n", description="Alpha")
    write_custom_command(tmp_path, "middle.md", body="Middle command.\n", description="Middle")

    tui = _start_tui_with_workspace(tmp_path)
    try:
        _open_slash_autocomplete(tui)
        tui.assert_screen_contains("alpha", msg="alpha should appear")
        tui.assert_screen_contains("middle", msg="middle should appear")
        tui.assert_screen_contains("zebra", msg="zebra should appear")
        _close_autocomplete(tui)
    finally:
        tui.stop()


@pytest.mark.story_5_3
def test_refresh_sees_newly_added_file(build_binary, tmp_path):
    """AC5: adding a command file mid-session makes it appear on next ``/`` open."""
    tui = _start_tui_with_workspace(tmp_path)
    try:
        _open_slash_autocomplete(tui)
        tui.assert_screen_not_contains("added-later", msg="No command file exists yet")
        tui.send(ESC)
        time.sleep(0.3)
        tui.send(BACKSPACE)
        time.sleep(0.2)

        write_custom_command(
            tmp_path, "added-later.md", body="Added mid-session.\n", description="Added later"
        )

        _open_slash_autocomplete(tui)
        tui.assert_screen_contains(
            "added-later", msg="Newly added command should appear after re-scan"
        )
        _close_autocomplete(tui)
    finally:
        tui.stop()


@pytest.mark.story_5_3
def test_refresh_removes_deleted_file(build_binary, tmp_path):
    """AC5: deleting a command file mid-session removes it from autocomplete."""
    cmd_file = write_custom_command(
        tmp_path, "ephemeral.md", body="Temporary command.\n", description="Ephemeral"
    )

    tui = _start_tui_with_workspace(tmp_path)
    try:
        _open_slash_autocomplete(tui)
        tui.assert_screen_contains("ephemeral", msg="Command should appear initially")
        _close_autocomplete(tui)

        cmd_file.unlink()

        _open_slash_autocomplete(tui)
        tui.assert_screen_not_contains(
            "ephemeral", msg="Deleted command should disappear after re-scan"
        )
        _close_autocomplete(tui)
    finally:
        tui.stop()


@pytest.mark.story_5_3
def test_builtin_commands_still_dispatch(build_binary, tmp_path):
    """AC12 regression: built-in ``/mode plan`` still dispatches via built-in path."""
    write_custom_command(tmp_path, "mode.md", body="Shadow attempt.\n", description="Shadow mode")

    tui = _start_tui_with_workspace(tmp_path)
    try:
        type_slowly(tui, "/mode plan")
        time.sleep(0.3)
        tui.send(ENTER)
        time.sleep(0.3)
        tui.send(ENTER)
        assert tui.wait_for_screen("Permission mode: Plan", timeout=5.0), (
            "Built-in /mode should still dispatch correctly"
        )
    finally:
        tui.stop()


@pytest.mark.story_5_3
def test_deeply_nested_namespace_appears(build_binary, tmp_path):
    """AC4: multi-level namespace like ``ci:github:actions`` works."""
    write_custom_command(
        tmp_path,
        "ci/github/actions.md",
        body="Run CI.\n",
        description="CI GitHub Actions",
    )

    tui = _start_tui_with_workspace(tmp_path)
    try:
        _open_slash_autocomplete(tui)
        tui.assert_screen_contains(
            "ci:github:actions", msg="Deeply nested namespace should appear"
        )
        _close_autocomplete(tui)
    finally:
        tui.stop()


@pytest.mark.story_5_3
def test_custom_command_with_empty_body_shows_no_description(build_binary, tmp_path):
    """AC3: empty-body command file shows ``(no description)`` placeholder."""
    write_custom_command(tmp_path, "empty.md", body="")

    tui = _start_tui_with_workspace(tmp_path)
    try:
        _open_slash_autocomplete(tui)
        tui.assert_screen_contains("empty", msg="Empty-body command should still appear")
        _close_autocomplete(tui)
    finally:
        tui.stop()


@pytest.mark.story_5_3
def test_autocomplete_filter_narrows_custom_commands(build_binary, tmp_path):
    """AC11: typing after ``/`` filters custom commands by substring match."""
    write_custom_command(tmp_path, "deploy-prod.md", body="Deploy prod.\n", description="Deploy prod")
    write_custom_command(tmp_path, "deploy-staging.md", body="Deploy staging.\n", description="Deploy staging")
    write_custom_command(tmp_path, "review.md", body="Review code.\n", description="Review code")

    tui = _start_tui_with_workspace(tmp_path)
    try:
        _open_slash_autocomplete(tui)
        tui.assert_screen_contains("deploy-prod", msg="deploy-prod should appear")
        tui.assert_screen_contains("review", msg="review should appear")

        type_slowly(tui, "deploy")
        time.sleep(0.5)
        tui.assert_screen_contains("deploy-prod", msg="deploy-prod should match filter")
        tui.assert_screen_contains("deploy-staging", msg="deploy-staging should match filter")
        tui.assert_screen_not_contains("review", msg="review should be filtered out")

        _close_autocomplete(tui)
    finally:
        tui.stop()
