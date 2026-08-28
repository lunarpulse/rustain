#!/usr/bin/env python3
"""Story 19.9 — Journey 0 (Sam) capture driver, pexpect half.

Walks PRD Journey 0 through the REAL binary under a PTY against the 19.7 scene
stub: first run with nothing but an env key, streaming, real `Read` tool blocks,
a `Write` that raises a real permission prompt, `y`, the **inline line diff** in
the expanded tool block (Story 19.1), `Ctrl+Q` (Story 19.3), and a second launch
that restores the session from `.meta.json`. The SHELL driver
(`journey-J0-sam.sh`) owns the stub lifecycle, the receipt header and the three
trailers (ledger / sessions / requests); this script owns the keystrokes, the
screen-state waits and the labelled pane dumps.

Evidence discipline (19.7 A4 / 18-4e A16 / this story's A2): the screen is a
WITNESS, with one deliberate exception — the expanded diff is a screen-STATE
marker whose load-bearing row is `- <old line>`, a string THIS SCRIPT wrote to
disk, the binary snapshotted, read back and rendered. The stub never serves it;
`grep -c J0-SAM-OLD-LINE scenes/j0-sam.json` is 0 and the gate checks that first.

Beats are pinned by the Task-0 discoveries (story Debug Log, 2026-08-28):

* There is **no `Glob` tool** in this product (the builtin catalogue is Bash /
  Edit / Read / Write / activate_skill / apply_patch / exit_plan_mode /
  propose_plan / remember / remember_fact / search_skills / search_tools /
  skill_view, 15 definitions on the wire). PRD J0's "explain this codebase
  structure" beat is therefore TWO real `Read`s over two driver-written files,
  not Read + Glob; the needle-came-back marker is `J0-SAM-LIB-NEEDLE`.
* The `Write` is the conversation's **fourth** tool call and lives in the second
  assistant turn as its **second** invocation — reaching it by keyboard is what
  Story 19.9's A3 fix makes possible, and the front-door proof of AC7.
* The diff renders AFTER execution, in the expanded block; the approval prompt
  renders the tool input only.

Usage (normally invoked by journey-J0-sam.sh):
    RUSTAIN_TUI_BINARY=<bin> python3 journey_j0_sam.py <workspace> <stub-url> <scratch-home>

Exit status: 0 only if every beat reached its pinned screen state.
"""

from __future__ import annotations

import hashlib
import os
import sys
import time
from pathlib import Path

RUSTAIN_SRC = Path(os.environ["RUSTAIN_TUI_BINARY"]).resolve().parents[2]
sys.path.insert(0, str(RUSTAIN_SRC / "tests_tui"))
sys.path.insert(0, str(RUSTAIN_SRC / "tests_tui" / "journeys"))

from _lib import focus_block, log, pane, quit_ctrl_q, require  # noqa: E402
from harness import RustainTUI  # noqa: E402
from keys import ENTER, ESC  # noqa: E402

PERSONA = "scene-sam"
TITLE = "Scene capture"  # the 19.7 stub serves this fixed title

# The workspace Sam's first run reads and then rewrites. Five short ASCII lines
# (A16): the diff stays `- old` / `+ new` with context, no elision, no cap.
# `J0-SAM-OLD-LINE` sits on the ONE line the scripted Write changes;
# `J0-SAM-FILE-NEEDLE` sits on line 1 so the stub's 120-char `last_user` clip
# cannot hide it.
MAIN_RS = (
    "fn main() { // J0-SAM-FILE-NEEDLE\n"
    "    let cfg = load();\n"
    "    let v = cfg.unwrap(); // J0-SAM-OLD-LINE\n"
    "    run(v);\n"
    "}\n"
)
MAIN_RS_AFTER = (
    "fn main() { // J0-SAM-FILE-NEEDLE\n"
    "    let cfg = load();\n"
    '    let v = cfg.expect("config"); // J0-SAM-NEW\n'
    "    run(v);\n"
    "}\n"
)
LIB_RS = "// J0-SAM-LIB-NEEDLE\npub fn run(_v: u32) {}\n"

# The Write is turn 2's SECOND invocation (turn 2 is Read → Write).
WRITE_TURN = 2
WRITE_INVOCATION = 1


def env_block(stub_url: str, home: Path) -> dict[str, str]:
    """The 19.7 A5 env block, verbatim in effect.

    `ANTHROPIC_AUTH_TOKEN` empty is *unset*, and it must be: `harness.start()`
    copies `rustain/.env`, which exports a real bearer token, and the auth
    precedence is AUTH_TOKEN > API_KEY. `HOME` is redirected because the
    user-global config layer is read from `dirs::home_dir()` and a developer's
    `[provider.*]` there would silently take the CONFIG path and ignore the stub
    — which would make this capture prove nothing about J0's env front door.
    """
    return {
        "ANTHROPIC_AUTH_TOKEN": "",
        "ANTHROPIC_API_KEY": PERSONA,
        "ANTHROPIC_BASE_URL": stub_url,
        "ANTHROPIC_DEFAULT_SONNET_MODEL": "",
        "RUSTAIN_MODELS_DEV_URL": stub_url,
        "OPENROUTER_API_KEY": "",
        "HOME": str(home),
        "NO_COLOR": "1",
    }


def launch(workspace: Path, stub_url: str, home: Path, *, fresh: bool) -> RustainTUI:
    tui = RustainTUI(
        fresh=fresh,
        build=False,
        workspace=workspace,
        allowed_tools=[],  # A4: nothing pre-allowed, or no prompt ever paints
        env_overrides=env_block(stub_url, home),
        timeout=60,
    ).start()
    # A4 / preflight: the harness copied `rustain/.env` into Sam's "no other
    # config" workspace. The env override already blanks the variable and
    # rustain has no dotenv loader, but a real credential file one `Read` away
    # from a committed pane dump is not something a receipt should contain.
    (workspace / ".env").unlink(missing_ok=True)
    return tui


def main() -> int:
    workspace, stub_url, home = (Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3]))

    (workspace / "src").mkdir(parents=True, exist_ok=True)
    (workspace / "src" / "main.rs").write_text(MAIN_RS)
    (workspace / "src" / "lib.rs").write_text(LIB_RS)
    home.mkdir(parents=True, exist_ok=True)

    # ── Launch 1 ─────────────────────────────────────────────────────────────
    tui = launch(workspace, stub_url, home, fresh=True)
    require(tui, "Ready", 30, "boot")
    pane(tui, "boot")  # ` anthropic/claude-sonnet-4-6`, `Normal`, `Ready`

    # ── Beat 1 — "explain this codebase structure": two real Read runs ───────
    tui.send_message("explain this codebase structure J0-SAM-MSG-1")
    require(tui, "Two source files", 60, "turn-1 reply")
    pane(tui, "tools-collapsed")  # two collapsed Read blocks, no diff rows

    # ── Beat 2 — vim nav, recorded, never asserted (A2) ──────────────────────
    tui.send(ESC)
    time.sleep(0.4)
    tui.send("k")
    time.sleep(0.3)
    tui.send("i")
    time.sleep(0.3)
    pane(tui, "vim-nav")

    # ── Beat 3 — the climax: Write → prompt → y → executed block ─────────────
    tui.send_message("refactor the error handling in main.rs")
    require(tui, "[y] Allow", 60, "write permission prompt")
    pane(tui, "write-prompt")  # `[y] Allow` + `Write:` — the tool INPUT only
    tui.send("y")
    require(tui, "Rewrote the error handling", 60, "turn-2 reply")
    require(tui, TITLE, 60, "auto-title on the status bar (second exchange)")

    # The file bytes, product-written. Both hashes are printed by THIS script,
    # so the pair is a positive control; the product-side half of the claim is
    # the follow-up request row (`tool_results:1`, `last_user` starting
    # `Successfully wrote`), which the gate reads from the request log.
    written = (workspace / "src" / "main.rs").read_text()
    print(
        f"[witness] src/main.rs sha256: {hashlib.sha256(written.encode()).hexdigest()}",
        flush=True,
    )
    print(
        f"[witness] scripted content sha256: {hashlib.sha256(MAIN_RS_AFTER.encode()).hexdigest()}",
        flush=True,
    )
    print(f"[witness] bytes written: {len(MAIN_RS_AFTER.encode())}", flush=True)

    tui.send(ESC)
    time.sleep(0.5)
    tui.send("g")
    time.sleep(0.8)
    pane(tui, "write-collapsed")  # `┄ ✓ Success Write "src/main.rs"`, no `+ ` row

    # ── Beat 4 — expand the FOURTH tool call by keyboard (AC7 front door) ────
    focus_block(
        tui, turn_index=WRITE_TURN, invocation_index=WRITE_INVOCATION, label="write block"
    )
    tui.send(ENTER)
    time.sleep(1.5)
    pane(tui, "write-expanded")  # `- …J0-SAM-OLD-LINE` and `+ …`, no fallback text
    tui.send(ENTER)  # re-collapse, so `Ctrl+Q` is not taken by a peek
    time.sleep(0.8)
    pane(tui, "write-recollapsed")

    quit_ctrl_q(tui)

    # ── Launch 2 — restore from .meta.json, no `--new` (A6) ──────────────────
    print("\n=== launch 2 — bare `rustain`, the session restores itself", flush=True)
    tui = launch(workspace, stub_url, home, fresh=False)
    require(tui, "Ready", 30, "boot after restore")
    require(tui, "J0-SAM-MSG-1", 30, "the first message repainted from disk")
    pane(tui, "after-restore")

    # Boot leaves Input focus; ONE Esc enters Chat focus (a second would toggle
    # straight back — see `_lib.focus_block`).
    tui.send(ESC)
    time.sleep(0.5)

    focus_block(
        tui,
        turn_index=WRITE_TURN,
        invocation_index=WRITE_INVOCATION,
        label="restored write block",
    )
    tui.send(ENTER)
    time.sleep(1.5)
    pane(tui, "restore-expanded")  # the persisted ToolOutput.diff still carries `- `
    tui.send(ENTER)
    time.sleep(0.8)

    quit_ctrl_q(tui)
    log("restore launch quit")
    print("[beats] complete", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
