#!/usr/bin/env python3
"""Story 19.9 — Journey 2 (Jordan the skeptic) capture driver, pexpect half.

Walks PRD Journey 2 through the REAL binary under a PTY against the 19.7 scene
stub: a `.claude/agents/*.md` file dropped in and activated with `@Agents/`, a
markdown findings reply, a `Bash` that raises a real permission prompt, a `Write`
that raises another, the **inline line diff** in the expanded tool block, a
sonnet↔haiku model switch and a provider switch onto a **second wire** — the
OpenAI-compat `/v1/chat/completions` arm the scene provider gained in this story
(ruling A11). The SHELL driver (`journey-J2-jordan.sh`) owns the stub lifecycle,
the receipt header and the three trailers; this script owns the keystrokes, the
screen-state waits and the labelled pane dumps.

Front door, deliberately different from J0's: J2 runs on the **config** provider
path (`.rustain/config.toml` with two stubbed providers), because a provider
switch has nothing to switch between on the env path. J0 keeps the env door —
that *is* J0's claim.

Evidence discipline (19.7 A4 / 18-4e A16 / A2): the screen is a witness, with
the one deliberate exception the diff row is (a string THIS SCRIPT wrote to
disk, snapshotted and rendered by the binary). Agent activation is asserted from
the `tools` count that reached the wire, never from the status bar. The model
switches are proved from the request log's `model`/`path` fields (gate J2); the
on-screen flashes are only switch-specific witnesses, named precisely because
they ACCUMULATE — a bare `Switched to` is satisfied by the first flash forever.

Usage (normally invoked by journey-J2-jordan.sh):
    RUSTAIN_TUI_BINARY=<bin> python3 journey_j2_jordan.py <workspace> <stub-url> <scratch-home>

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

from _lib import (  # noqa: E402
    focus_block,
    log,
    pane,
    quit_ctrl_q,
    require,
    require_gone,
    sha256_of,
)
from fixtures.agents import write_custom_agent  # noqa: E402
from harness import RustainTUI  # noqa: E402
from keys import CTRL_X, DOWN, ENTER, ESC, RIGHT, TAB, UP  # noqa: E402

PERSONA = "scene-jordan"
OR_PERSONA = "scene-jordan-or"
TITLE = "Scene capture"

# Six short ASCII lines (A16): the diff stays `- old` / `+ new` with context.
# `J2-JORDAN-OLD-LINE` is on the ONE line the scripted Write replaces;
# `J2-JORDAN-FILE-NEEDLE` is on a line the Write keeps, and early enough that
# the stub's 120-char `last_user` clip cannot hide it.
PARSE_RS = (
    "pub fn parse_port(s: &str) -> u32 {\n"
    "    // J2-JORDAN-FILE-NEEDLE\n"
    "    let n = s.parse::<u32>().unwrap(); // J2-JORDAN-OLD-LINE\n"
    "    n\n"
    "}\n"
    "// auth parse helper\n"
)
PARSE_RS_AFTER = (
    "pub fn parse_port(s: &str) -> u32 {\n"
    "    // J2-JORDAN-FILE-NEEDLE\n"
    "    let n = s.parse::<u32>().unwrap_or(0); // J2-NEW\n"
    "    n\n"
    "}\n"
    "// auth parse helper\n"
)

# Both target blocks are the ONLY invocation of their assistant turn, so the
# within-turn `Tab` cycle cannot select them (`CycleInvocationInFocusedTurn`
# needs count >= 2 — measured at Task 0). `]]` to the turn is what seats focus on
# them: the Read is assistant turn 1's block, the Write is assistant turn 3's.
READ_TURN = 1
WRITE_TURN = 3


def write_config(workspace: Path, stub_url: str) -> None:
    """The A11 config provider path, written BEFORE `start()`.

    `harness.start()` only creates `.rustain/config.toml` when it is absent, so
    this block — not the harness default — is what `init_provider_layer` reads.
    Two providers, both pointed at the same stub process:

    * `anthropic` (kind `anthropic`) answers `POST {base_url}/v1/messages`;
    * `openrouter` (kind `openrouter`, i.e. the OpenAI-compat adapter) answers
      `POST {base_url}/chat/completions` with `base_url` ending in `/v1`.

    ⚑ The KEY NAMES matter, pinned at Task 0: `init_provider_layer` makes the
    FIRST enabled provider active (`first_enabled_id`) and `AppConfig.provider`
    iterates in key order, so `[provider.scene]` would have booted `openrouter`
    as the active delegate and sent T1 down the OpenAI wire. `anthropic` sorts
    before `openrouter`, which is what makes the sonnet rows sonnet and leaves
    the provider switch something to switch TO.

    `always_tools = []` keeps the permission gate real (A4).
    ⛔ The env block must NOT also set `ANTHROPIC_BASE_URL`/`ANTHROPIC_API_KEY`:
    two front doors in one capture prove neither (19.7 A5). The key value the
    stub sees is `SCENE_API_KEY`'s — its name contains `API_KEY`, so the factory
    picks the `x-api-key` scheme and the persona resolves to `scene-jordan`.
    """
    rustain_dir = workspace / ".rustain"
    rustain_dir.mkdir(parents=True, exist_ok=True)
    (rustain_dir / "config.toml").write_text(
        "[permissions]\n"
        "always_tools = []\n"
        "\n"
        "[provider.anthropic]\n"
        'kind = "anthropic"\n'
        'provider_id = "anthropic"\n'
        'model_id = "claude-sonnet-4-6"\n'
        'api_key_env = "SCENE_API_KEY"\n'
        f'base_url = "{stub_url}"\n'
        "enabled = true\n"
        "\n"
        "[provider.openrouter]\n"
        'kind = "openrouter"\n'
        'provider_id = "openrouter"\n'
        'model_id = "anthropic/claude-sonnet-4.6"\n'
        'api_key_env = "OPENROUTER_API_KEY"\n'
        f'base_url = "{stub_url}/v1"\n'
        "enabled = true\n"
    )


def env_block(stub_url: str, home: Path) -> dict[str, str]:
    """The 19.7 block MINUS the Anthropic env door, PLUS the two config keys."""
    return {
        "ANTHROPIC_AUTH_TOKEN": "",
        "ANTHROPIC_API_KEY": "",
        "ANTHROPIC_BASE_URL": "",
        "ANTHROPIC_DEFAULT_SONNET_MODEL": "",
        "RUSTAIN_MODELS_DEV_URL": stub_url,
        "SCENE_API_KEY": PERSONA,
        "OPENROUTER_API_KEY": OR_PERSONA,
        "HOME": str(home),
        "NO_COLOR": "1",
    }


def open_model_overlay(tui) -> None:
    """`Ctrl+X` `m` — there is no `/model` and no `/provider` slash command."""
    tui.send(CTRL_X)
    time.sleep(0.5)
    tui.send("m")
    require(tui, "Select Model", 15, "model switcher overlay open")


def main() -> int:
    workspace, stub_url, home = (Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3]))

    # ── Jordan's workspace: three agent files and the file he wants audited ───
    write_custom_agent(
        workspace,
        "rust-auditor",
        "Audits Rust code for security issues",
        body="You are a security auditor. Report findings as markdown.\n",
        allowed_tools=["Read", "Grep", "Glob", "Bash", "Write"],
    )
    write_custom_agent(workspace, "migration-checker", "Checks migrations")
    write_custom_agent(
        workspace,
        "perf-profiler",
        "Quick performance checks",
        model="claude-haiku-4-5",
    )
    (workspace / "src" / "auth").mkdir(parents=True, exist_ok=True)
    (workspace / "src" / "auth" / "parse.rs").write_text(PARSE_RS)
    home.mkdir(parents=True, exist_ok=True)

    write_config(workspace, stub_url)

    tui = RustainTUI(
        fresh=True,
        build=False,
        workspace=workspace,
        allowed_tools=[],  # A4 — nothing pre-allowed
        env_overrides=env_block(stub_url, home),
        timeout=60,
    ).start()
    (workspace / ".env").unlink(missing_ok=True)  # A4 — the copied real credential
    require(tui, "Ready", 30, "boot")
    pane(tui, "boot")

    # ── Beat 1 — the agent file reaches the wire as a TOOL FILTER ────────────
    # Discovery must land before the mention, or activation is
    # `AgentDiscoveryPending` and the filter never reaches the request.
    # ⚑ The `Discovered 3 custom agent(s)` notice is a FLASH: measured at Task 0,
    # it has already expired by the time the boot screen settles on `Ready`, so
    # waiting for it is a guaranteed timeout. The popup listing the agent is the
    # same fact and it is a persistent screen state — `refresh_agent_suggestions`
    # runs on `AgentsDiscovered`, so a popup row for `rust-auditor` cannot appear
    # before discovery completed.
    for c in "@Agents/rust-au":
        tui.send(c)
        time.sleep(0.06)
    require(tui, "@Agents/rust-auditor", 20, "agent popup lists the auditor")
    pane(tui, "agent-popup")
    # ⚑ `Down` FIRST, pinned at Task 0: the popup's first row is always the
    # synthetic `@Agents/default` ("Clear active agent"), and `Tab` accepts the
    # SELECTED row. Without it the mention resolves to `default`, the activation
    # is a CLEAR, the tool filter never reaches the wire, and the turn still
    # succeeds — a false green that only the `tools` count would have caught.
    tui.send(DOWN)
    time.sleep(0.4)
    tui.send(TAB)  # accepts the selected row; does not submit
    time.sleep(0.5)
    require(tui, "@Agents/rust-auditor", 10, "mention accepted into the input")
    tui.send_message(" audit src/auth for security issues")
    # The `Active agent: …` notice is a FLASH (measured at Task 0). The status-bar
    # `Agent: …` segment is the persistent screen state, and it is a witness —
    # the asserted evidence is the `tools` count on the wire (A9).
    require(tui, "Agent: rust-auditor", 30, "agent activated")
    pane(tui, "agent-active")

    require(tui, "Findings", 60, "markdown audit reply")
    pane(tui, "audit-reply")  # fence content + heading TEXT (styling is invisible)

    # ── Beat 2 — every Read block is expandable too (A3, first invocation) ───
    tui.send(ESC)
    time.sleep(0.5)
    focus_block(tui, turn_index=READ_TURN, label="read block")
    tui.send(ENTER)
    time.sleep(1.2)
    pane(tui, "read-expanded")  # `┌─ Read ` + a gutter line carrying the file needle
    tui.send(ENTER)
    time.sleep(0.6)
    tui.send("i")
    time.sleep(0.4)

    # ── Beat 3 — Bash: a real process, a real prompt, its stdout round-tripped ─
    tui.send_message("check the advisory database for this crate")
    require(tui, "[y] Allow", 60, "bash permission prompt")
    pane(tui, "bash-prompt")  # `[y] Allow` + `Bash:`
    tui.send("y")
    require(tui, "No advisories apply offline", 60, "bash reply")
    require(tui, TITLE, 60, "auto-title on the status bar (second exchange)")

    # ── Beat 4 — the climax: Write → prompt → y → diff in the expanded block ─
    tui.send_message("propose a fix")
    require(tui, "[y] Allow", 60, "write permission prompt")
    pane(tui, "fix-prompt")  # `[y] Allow` + `Write:`
    tui.send("y")
    require(tui, "Fix applied", 60, "write reply")

    # The file bytes, product-written. Both hashes are printed by THIS script
    # AND compared (review finding 2026-08-29): the expanded diff row proves
    # the change was RENDERED, this pair proves the scripted bytes actually
    # LANDED — the request log only ever saw `Successfully wrote N bytes` come
    # back, never which bytes they were. The gate consumes this script's exit
    # code, so a mismatch fails the run after the pane below records what the
    # screen still claimed.
    on_disk = sha256_of(workspace / "src" / "auth" / "parse.rs")
    scripted = hashlib.sha256(PARSE_RS_AFTER.encode()).hexdigest()
    print(f"[witness] src/auth/parse.rs sha256: {on_disk}", flush=True)
    print(f"[witness] scripted content sha256: {scripted}", flush=True)
    print(f"[witness] bytes written: {len(PARSE_RS_AFTER.encode())}", flush=True)
    if on_disk != scripted:
        pane(tui, "FAILED-witness-sha256")
        print(
            "FAIL: product wrote different bytes than the script served — "
            f"on-disk {on_disk} != scripted {scripted}",
            flush=True,
        )
        sys.exit(1)
    print(f"[witness] on-disk bytes == scripted content ({on_disk})", flush=True)

    tui.send(ESC)
    time.sleep(0.5)
    tui.send("g")
    time.sleep(0.8)
    focus_block(tui, turn_index=WRITE_TURN, label="write block")
    # Dumped AFTER positioning, so the collapsed Write line is guaranteed to be
    # inside the 30-row viewport this pane records.
    pane(tui, "fix-collapsed")  # `┄ ✓ Success Write "src/auth/parse.rs"`, no `+ ` row
    tui.send(ENTER)
    time.sleep(1.5)
    pane(tui, "fix-expanded")  # `- …J2-JORDAN-OLD-LINE` and `+ …`, no `┌─ Read `
    tui.send(ENTER)
    time.sleep(0.8)
    tui.send("i")
    time.sleep(0.4)

    # ── Beat 5 — sonnet ↔ haiku, proved from the request log's `model` ───────
    open_model_overlay(tui)
    pane(tui, "model-switcher-open")
    tui.send(DOWN)
    time.sleep(0.4)
    tui.send(ENTER)
    # ⚑ The flashes ACCUMULATE on screen (receipt J2: all three visible at
    # once), so a bare `Switched to` is satisfied by the FIRST switch even
    # when a later one silently no-ops — each wait names the
    # switch-DISTINGUISHING substring instead. Witnesses only: the switch
    # proof stays in the request log's per-row `model`/`path` (gate J2).
    require(tui, "Switched to anthropic/Claude Haiku", 20, "model switch flash")
    pane(tui, "model-switched")
    tui.send_message("quick check: is the fix complete?")
    require(tui, "Complete.", 60, "haiku-side reply")

    open_model_overlay(tui)
    tui.send(UP)  # Up — back to the row the overlay opened on
    time.sleep(0.4)
    tui.send(ENTER)
    require(tui, "Switched to anthropic/Claude Sonnet", 20, "model switch back flash")
    tui.send_message("explain the reasoning in detail")
    require(tui, "Detailed reasoning follows", 60, "sonnet-side reply")

    # ── Beat 6 — the provider switch: a different wire, same UI (A11) ────────
    open_model_overlay(tui)
    tui.send(RIGHT)  # to the openrouter column
    time.sleep(0.5)
    pane(tui, "model-switcher-openrouter")
    tui.send(ENTER)
    require(tui, "Switched to openrouter/", 25, "provider switch flash")
    pane(tui, "provider-switched")
    tui.send_message("same question, different provider")
    require(tui, "Same answer, other wire", 60, "openai-wire reply")
    pane(tui, "openrouter-reply")

    tui.send(ESC)
    time.sleep(0.4)
    require_gone(tui, "Select Model", 5, "no overlay is open before Ctrl+Q")
    quit_ctrl_q(tui)
    log("J2 quit via Ctrl+Q")
    print("[beats] complete", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
