#!/usr/bin/env python3
"""Story 19.8 — Journey 1 (Kai) capture driver, pexpect half.

Walks Kai's day through the REAL binary under a PTY against the 19.7 scene
stub: multi-tab, auto-title, `@file` mention, `/command`, plan card + `y`,
fork at a message, model switcher, context meter. The SHELL driver
(`journey-J1-kai.sh`) owns the stub lifecycle, the receipt header and the
three trailers (ledger / sessions / requests); this script owns the
keystrokes, the screen-state waits and the labelled pane dumps.

Evidence discipline (19.7 A4 / 18.4e): the screen is a WITNESS. What the gate
asserts on is the stub's request log, the session files and the usage ledger —
all printed by the shell driver after this script exits. Every pane dump below
is a reading aid except where `gate J1` slices it for a screen-STATE marker
(overlay open/closed, `ctx:` meter, sidebar attempt).

Beats are pinned by the Task-0 discoveries (story Debug Log, 2026-08-26):
the title call fires at the TurnComplete of the SECOND user exchange, so the
wait-for-title belongs after the `@file` turn; the fork targets message 1
(`g` jumps to top) because forking at the tail right after plan execution is
`DF-19-8-FORK-TAIL-OOB`; the History sidebar is NOT CAPTURED in this tree
(`DF-19-8-CTRLH-PTY`) — the palette attempt below records that state honestly
instead of pretending.

Usage (normally invoked by journey-J1-kai.sh):
    RUSTAIN_TUI_BINARY=<bin> python3 journey_j1_kai.py <workspace> <stub-url> <scratch-home>

Exit status: 0 only if every beat reached its pinned screen state.
"""

from __future__ import annotations

import os
import sys
import time
from pathlib import Path

RUSTAIN_SRC = Path(os.environ["RUSTAIN_TUI_BINARY"]).resolve().parents[2]
sys.path.insert(0, str(RUSTAIN_SRC / "tests_tui"))

from harness import RustainTUI  # noqa: E402
from keys import CTRL_T, CTRL_X, ENTER, ESC, TAB  # noqa: E402

PERSONA = "scene-kai"
NEEDLE = "J1-KAI-NEEDLE"
TITLE = "Scene capture"  # 19.7's stub serves this fixed title (owner ruling A, 2026-08-26)


def log(msg: str) -> None:
    print(f"[beat] {msg}", flush=True)


def pane(tui: RustainTUI, label: str) -> None:
    print(f"--- pane: {label} ---", flush=True)
    print(tui.get_screen_text(), flush=True)
    print("--- end pane ---", flush=True)


def require(tui: RustainTUI, needle: str, timeout: float, label: str) -> None:
    """wait_for_screen ONLY (A3: wait_for_idle cannot fail and is forbidden)."""
    if not tui.wait_for_screen(needle, timeout=timeout):
        pane(tui, f"FAILED-{label}")
        print(f"FAIL: never saw {needle!r} — {label}", flush=True)
        sys.exit(1)
    log(f"{label}: saw {needle!r}")


def require_gone(tui: RustainTUI, needle: str, timeout: float, label: str) -> None:
    if not tui.wait_for_screen_not_contains(needle, timeout=timeout):
        pane(tui, f"FAILED-{label}")
        print(f"FAIL: {needle!r} never left the screen — {label}", flush=True)
        sys.exit(1)
    log(f"{label}: {needle!r} gone")


def main() -> int:
    workspace, stub_url, home = (Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3]))

    # ── Kai's workspace: the file he @-mentions and the command he runs ──────
    (workspace / "notes").mkdir(parents=True, exist_ok=True)
    (workspace / ".claude" / "commands").mkdir(parents=True, exist_ok=True)
    (workspace / "notes" / "auth.md").write_text(
        f"{NEEDLE} auth refactor notes — session middleware is stale on /auth/refresh.\n"
    )
    (workspace / ".claude" / "commands" / "deploy-staging.md").write_text(
        "Run the staging deploy playbook.\n"
    )
    home.mkdir(parents=True, exist_ok=True)

    # The 19.7 A5 env block, verbatim in effect (SceneStub.env() is the
    # authoritative copy; .env would otherwise override inherited env, and
    # AUTH_TOKEN > API_KEY would leak the developer's real bearer token).
    env_overrides = {
        "ANTHROPIC_AUTH_TOKEN": "",
        "ANTHROPIC_API_KEY": PERSONA,
        "ANTHROPIC_BASE_URL": stub_url,
        "ANTHROPIC_DEFAULT_SONNET_MODEL": "",
        "RUSTAIN_MODELS_DEV_URL": stub_url,
        "OPENROUTER_API_KEY": "",
        "HOME": str(home),
        "NO_COLOR": "1",
    }

    tui = RustainTUI(
        fresh=True,
        build=False,
        workspace=workspace,
        env_overrides=env_overrides,
        timeout=60,
    ).start()
    require(tui, "Ready", 30, "boot")

    # ── Beat 1 — message 1: the ctx meter moves on the first turn ────────────
    tui.send_message("start the auth refactor review")
    require(tui, "notes are loaded", 30, "turn-1 reply")
    pane(tui, "after-turn-1")  # gate J1 slices this for `ctx: ` + `%`

    # ── Beat 2 — @file mention (Tab accepts the dropdown entry) ──────────────
    tui.send("@")
    time.sleep(0.4)
    for c in "notes/au":
        tui.send(c)
        time.sleep(0.06)
    require(tui, "notes/auth.md", 5, "file dropdown lists the mention")
    pane(tui, "file-dropdown")
    tui.send(TAB)
    # The dropdown lists the bare path; the accepted mention carries the '@'.
    require(tui, "@notes/auth.md", 5, "mention accepted into the input")
    pane(tui, "file-after-tab")
    tui.send_message(" review this please")
    require(tui, "Three routes carry stale middleware", 30, "turn-2 reply")

    # ── The auto-title lands after the SECOND exchange (Task-0 pin) ──────────
    require(tui, TITLE, 30, "auto-title on the status bar")
    pane(tui, "after-title")

    # ── Beat 3 — /deploy-staging (Enter selects the suggestion, Enter submits)─
    for c in "/deploy-staging":
        tui.send(c)
        time.sleep(0.05)
    # The dropdown renders the command's description line; the bare command
    # name is already on screen in the input being typed, so it cannot prove
    # the dropdown opened.
    require(tui, "Run the staging deploy playbook.", 5, "slash dropdown")
    pane(tui, "slash-dropdown")
    tui.send(ENTER)
    time.sleep(0.5)
    tui.send(ENTER)
    require(tui, "Deploy-staging command body received", 30, "turn-3 reply")

    # ── Beat 4 — plan card, y to approve, one post-approval executor POST ────
    tui.send_message("propose a plan for the auth refactor")
    require(tui, "Plan: Auth refactor", 30, "plan card raised")
    pane(tui, "plan-card")
    tui.send("y")
    require_gone(tui, "Plan: Auth refactor", 10, "plan card resolved")
    require(tui, "Plan complete", 30, "plan executed")
    pane(tui, "after-plan-exec")

    # ── Beat 5 — fork at message 1 (g jumps to top; tail-fork is DF'd) ───────
    tui.chat_mode()
    tui.send("g")
    time.sleep(0.8)
    tui.send("f")
    require(tui, "Fork conversation", 10, "fork overlay at message 1")
    pane(tui, "fork-overlay")
    tui.send("y")
    require(tui, "Forked from", 10, "fork flash")
    time.sleep(1.5)
    pane(tui, "after-fork")

    # ── Beat 6 — new tab, one exchange there (no title call fires for it) ────
    tui.send(CTRL_T)
    time.sleep(1.2)
    pane(tui, "tab2-open")
    tui.send_message("second tab needle message")
    require(tui, "Tab two replies", 30, "turn on the second tab")
    pane(tui, "tab2-done")

    # ── Back to tab 1 (Chat focus, number key), then the model switcher ──────
    tui.chat_mode()
    tui.send("1")
    time.sleep(0.8)
    tui.send(CTRL_X)
    time.sleep(0.4)
    tui.send("m")
    require(tui, "Select Model", 10, "model switcher overlay open")
    pane(tui, "model-switcher-open")  # gate J1 slices open/closed pair; NO model name is a needle (A7)
    tui.send(ESC)
    require_gone(tui, "Select Model", 10, "model switcher closed")
    pane(tui, "model-switcher-closed")

    # ── Sidebar attempt — NOT CAPTURED in this tree (DF-19-8-CTRLH-PTY) ──────
    # Raw \x08 is Backspace under a PTY (never opens it, and kills input focus
    # if sent before the first message); the palette route below is the A5
    # workaround and IT no-ops in this tree too. Recorded, not hidden.
    tui.send("\x10")  # Ctrl+P
    if not tui.wait_for_screen("Command Palette", timeout=3.0):
        pane(tui, "FAILED-palette-open")
        print("FAIL: the command palette did not open for the sidebar attempt", flush=True)
        sys.exit(1)
    for c in "toggle sidebar":
        tui.send(c)
        time.sleep(0.05)
    time.sleep(0.8)
    tui.send(ENTER)
    time.sleep(1.2)
    pane(tui, "sidebar")  # gate J1 asserts the History header here → RED by design (A1)
    print(
        f"[witness] sidebar after palette 'toggle sidebar': "
        f"{'History panel PRESENT' if 'History' in tui.get_screen_text() else 'History panel ABSENT — NOT CAPTURED (DF-19-8-CTRLH-PTY)'}",
        flush=True,
    )

    # ── Quit (Ctrl+Q, Story 19.3) ─────────────────────────────────────────────
    tui.send("\x11")
    quit_deadline = time.monotonic() + 10
    while tui.child.isalive() and time.monotonic() < quit_deadline:
        time.sleep(0.2)
    if tui.child.isalive():
        tui.stop()
        print("FAIL: Ctrl+Q did not quit the TUI", flush=True)
        sys.exit(1)
    log("TUI quit via Ctrl+Q")
    print("[beats] complete", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
