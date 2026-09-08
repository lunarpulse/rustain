"""Shared pexpect helpers for the journey drivers (Stories 19.9 and 19.12).

`journey_j0_sam.py` and `journey_j2_jordan.py` both need the same four things:
a failing wait (`wait_for_idle` cannot fail and is forbidden — 19.8 A3), a
labelled pane dump, a `Ctrl+Q` with a liveness deadline, and a deterministic way
to put keyboard focus on a NAMED tool block rather than on whichever one the
renderer happens to pick.

`journey_j8_ravi.py` (Story 19.12) added a fifth: the CLI. It is the first
driver in the tree that drives `rustain` as a command rather than as a TUI, so
`cli_pane` and `cli_wizard_pane` live here too — see the block above them for
why they emit the TUI's pane fence around child output.

⛔ Nothing here asserts. A gate assertion reads the stub's request log, the
session files, the usage ledger, or a labelled pane slice out of the receipt —
never a string this module printed about itself (18-4e A16).

⛔ This module does not touch `harness.py` or `keys.py`: a journey driver that
needs a helper writes it here (story Scope boundary).
"""

from __future__ import annotations

import os
import subprocess
import sys
import time
from pathlib import Path


def log(msg: str) -> None:
    print(f"[beat] {msg}", flush=True)


def pane(tui, label: str) -> None:
    """Dump the whole screen between two labelled fences.

    `check-epic-close-gates.sh`'s `pane_expect`/`pane_refute` slice on exactly
    these two lines, so a needle that appears in another pane cannot satisfy a
    marker about this one.
    """
    print(f"--- pane: {label} ---", flush=True)
    print(tui.get_screen_text(), flush=True)
    print("--- end pane ---", flush=True)


# ── The CLI half (Story 19.12, rulings A11 + A12) ───────────────────────────
#
# J8 is the FIRST journey driver in the tree that drives the CLI rather than the
# TUI. `RustainTUI` is unusable for a CLI beat — it blocks on a TUI `Ready` wait,
# builds a pyte screen, copies `rustain/.env` and scaffolds a permissions file —
# and `harness.py` is off-limits, so the CLI spawn lives here (`_lib.py:13-15`
# sanctions exactly this).
#
# ⚠ A CLI capture has no pane, and these helpers emit the pane fence anyway.
# That is deliberate (ruling A11): `pane_slice` is the checker's ONE extractor
# and `pane_expect`/`pane_refute`/`pane_grep` all consume it, so emitting the
# same fence bytes around raw child output costs the checker zero helper edits.
# ⛔ No second extractor and no rename. The word "pane" is therefore a lie of
# convenience for J8, and this comment is where it is written down instead of
# shipped silently: for J8, a "pane" is a captured CLI output block.
#
# ⚠ `pane_slice` returns 42 on a DUPLICATE label, so every J8 label is unique.


def _fence(label: str, body: str) -> None:
    print(f"--- pane: {label} ---", flush=True)
    print(body, flush=True)
    print("--- end pane ---", flush=True)


def cli_pane(
    binary: Path,
    args: list[str],
    *,
    label: str,
    config_dir: Path,
    cwd: Path,
    env_extra: dict[str, str] | None = None,
    timeout: float = 60.0,
    expect_exit: int | None = None,
) -> tuple[int, str]:
    """Run one NON-INTERACTIVE `rustain` invocation and fence its output.

    stdin is `/dev/null` on purpose: it is what makes the `profile create`
    negative control a real non-TTY run, and every other CLI beat here is
    non-interactive by design (the driver installs `devops` exactly ONCE, so
    `import`'s un-TTY-guarded overwrite prompt is never reached — A22).

    Both streams are captured and fenced together. `export <n> -o <file>` prints
    its success line on stdout while `export <n>` prints it on stderr, and a
    fence carrying only one of them would lose a product-minted sentence.

    ⛔ The timeout is a FAILED RUN, never a skip: a wait that cannot fail is the
    trap this whole directory exists to avoid.
    """
    env = dict(os.environ)
    env["RUSTAIN_CONFIG_DIR"] = str(config_dir)
    env["NO_COLOR"] = "1"
    if env_extra:
        env.update(env_extra)
    try:
        proc = subprocess.run(
            [str(binary), *args],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            env=env,
            cwd=str(cwd),
            timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        _fence(label, f"(TIMED OUT after {timeout}s)")
        print(f"FAIL: `rustain {' '.join(args)}` did not finish — {label}", flush=True)
        sys.exit(1)
    body = (
        f"$ rustain {' '.join(args)}\n"
        f"{proc.stdout}{proc.stderr}"
        f"exit={proc.returncode}"
    )
    _fence(label, body)
    if expect_exit is not None and proc.returncode != expect_exit:
        print(
            f"FAIL: `rustain {' '.join(args)}` exited {proc.returncode}, "
            f"expected {expect_exit} — {label}",
            flush=True,
        )
        sys.exit(1)
    log(f"{label}: exit {proc.returncode}")
    return proc.returncode, proc.stdout + proc.stderr


def cli_wizard_pane(
    binary: Path,
    args: list[str],
    script: list[tuple[str, str]],
    *,
    label: str,
    config_dir: Path,
    cwd: Path,
    timeout: float = 60.0,
) -> tuple[int, str]:
    """Drive an INTERACTIVE `rustain` invocation under a real PTY and fence it.

    `script` is an ordered list of `(prompt, answer)` pairs. Prompts are matched
    with `expect_exact`, because `cli/profile/prompt.rs` writes them with
    `print!` + flush + `read_line` — no pyte needed, and no screen to poll.

    ⚠ pexpect defaults to `echo=True`, so every answer typed here is echoed by
    the PTY back into THIS pane. A marker that greps for one of those answers
    would be reading the driver's own keystrokes, not the product's output —
    which is why marker 2's needles live in the `profile show` pane and assert
    the padded `{label:<12}` column format a typed answer cannot produce (A23d).

    ⛔ Every wait can time out, and a timeout exits non-zero.
    """
    import io

    import pexpect

    env = dict(os.environ)
    env["RUSTAIN_CONFIG_DIR"] = str(config_dir)
    env["NO_COLOR"] = "1"
    captured = io.StringIO()
    child = pexpect.spawn(
        str(binary),
        args=args,
        env=env,
        cwd=str(cwd),
        encoding="utf-8",
        timeout=timeout,
        dimensions=(30, 130),
    )
    child.logfile_read = captured
    failure = ""
    try:
        for prompt, answer in script:
            child.expect_exact(prompt)
            child.sendline(answer)
        child.expect(pexpect.EOF)
    except pexpect.TIMEOUT:
        failure = f"timed out waiting for {prompt!r}"
    except pexpect.EOF:
        failure = f"the process exited before {prompt!r}"
    child.close()
    body = f"$ rustain {' '.join(args)}\n{captured.getvalue()}exit={child.exitstatus}"
    _fence(label, body)
    if failure:
        print(f"FAIL: {failure} — {label}", flush=True)
        sys.exit(1)
    if child.exitstatus != 0:
        print(
            f"FAIL: the wizard exited {child.exitstatus} (signal "
            f"{child.signalstatus}) — {label}",
            flush=True,
        )
        sys.exit(1)
    log(f"{label}: wizard completed, exit {child.exitstatus}")
    return child.exitstatus, captured.getvalue()


def require(tui, needle: str, timeout: float, label: str) -> None:
    """`wait_for_screen` ONLY: it can time out, and a timeout is a failed run."""
    if not tui.wait_for_screen(needle, timeout=timeout):
        pane(tui, f"FAILED-{label}")
        print(f"FAIL: never saw {needle!r} — {label}", flush=True)
        sys.exit(1)
    log(f"{label}: saw {needle!r}")


def require_gone(tui, needle: str, timeout: float, label: str) -> None:
    if not tui.wait_for_screen_not_contains(needle, timeout=timeout):
        pane(tui, f"FAILED-{label}")
        print(f"FAIL: {needle!r} never left the screen — {label}", flush=True)
        sys.exit(1)
    log(f"{label}: {needle!r} gone")


def focus_block(tui, *, turn_index: int, invocation_index: int = 0, label: str) -> None:
    """Put keyboard focus on the `invocation_index`-th tool block of the
    `turn_index`-th assistant turn, and leave it focused for `Enter`.

    ⛔ The caller MUST already be in **Chat** focus. `Esc` in Chat focus toggles
    BACK to Input (`app.rs`; `tests/keyboard.rs` pins it as "existing
    behavior"), so a defensive `Esc` here would bounce out of Chat and type the
    rest of this route into the message box — measured at Task 0, and the reason
    this function does not send one.

    Route, pinned at Task 0 T0.3(1) against the real binary:

    * ``g`` — jump to the top, so the walk starts from a known anchor.
    * ``]]`` × `turn_index` — `JumpProseAnchor(Down)` sets
      `view_state.focused_turn` to that assistant turn. `chat_pane`'s focus rule
      then seats `state.focused_tool_id` on that turn's FIRST visible tool
      block, which is why `invocation_index == 0` needs no further keystroke.
    * ``Tab`` × `invocation_index` — `CycleInvocationInFocusedTurn` advances
      one invocation per press FROM wherever focus already sits, and the ``]]``
      above already seated it on invocation 0, so reaching invocation k takes
      exactly k presses. ⛔ Tab short-circuits on `count >= 2`, so it is only
      sent when a later invocation is actually being asked for — and one press
      too many wraps modulo the invocation count, silently landing back on the
      first block (measured at Task 0).
    * The caller then sends ``Enter`` (`Chat.TOGGLE_TOOL_BLOCK`).

    ⚑ Both halves of this route only work BECAUSE of the Story 19.9 A3 fix.
    Before it, `chat_pane::find_focused_tool_id` overwrote
    `state.focused_tool_id` with the conversation's FIRST tool call id on EVERY
    frame: the `]]` seat never happened and the `Tab` selection was undone
    before `Enter` could act. Reverting the fix makes this sequence open the
    first `Read` instead — the story's front-door mutant, visible in the
    receipt as `┌─ Read ` in an expanded pane that should carry a diff.

    Turn indices are 1-based (the first ``]]`` reaches assistant turn 1);
    invocation indices are 0-based, matching the scene tables in the story.
    """
    tui.send("g")
    time.sleep(0.7)
    for _ in range(turn_index):
        tui.send("]")
        tui.send("]")
        time.sleep(0.5)
    # Invocation 0 is already seated by the `]]` above; each Tab advances one.
    for _ in range(invocation_index):
        tui.send("\t")
        time.sleep(0.5)
    log(f"{label}: focused turn {turn_index}, invocation {invocation_index}")


def quit_ctrl_q(tui, timeout: float = 10.0) -> None:
    """`Ctrl+Q` (Story 19.3) with a liveness deadline — and a checked STATUS.

    ⚠ Not honoured with an overlay or a Confirmation open (19.3 A4): close or
    answer first. Sent as the raw byte because `keys.py` has no constant for it
    and this module may not add one.

    The status is read, not assumed (review finding 2026-08-29): a screen wait
    polls a frozen pyte screen and stays green, so a binary that had already
    crashed before the byte — or that dies *on* it by signal — must fail HERE,
    not ride into a committed receipt as a clean quit. pexpect 4.9 populates
    `exitstatus`/`signalstatus` inside `isalive()` itself when it returns
    False (`pty_spawn.isalive` copies them from `ptyproc`), so reading them
    after the loop needs no `close()` — and none is sent, which leaves the
    harness's `stop()` (it closes a live child only) free to act unchanged.
    """
    if not tui.child.isalive():
        print(
            "FAIL: TUI was already dead before Ctrl+Q — the quit did not cause the exit",
            flush=True,
        )
        sys.exit(1)
    tui.send("\x11")
    deadline = time.monotonic() + timeout
    while tui.child.isalive() and time.monotonic() < deadline:
        time.sleep(0.2)
    if tui.child.isalive():
        tui.stop()
        print("FAIL: Ctrl+Q did not quit the TUI", flush=True)
        sys.exit(1)
    if tui.child.signalstatus is not None:
        print(
            f"FAIL: TUI died on Ctrl+Q by signal {tui.child.signalstatus}", flush=True
        )
        sys.exit(1)
    if tui.child.exitstatus != 0:
        print(f"FAIL: TUI exited {tui.child.exitstatus} on Ctrl+Q, not 0", flush=True)
        sys.exit(1)
    log(f"TUI quit via Ctrl+Q (exit status {tui.child.exitstatus})")


def sha256_of(path: Path) -> str:
    import hashlib

    return hashlib.sha256(path.read_bytes()).hexdigest()
