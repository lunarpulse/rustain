#!/usr/bin/env python3
"""Story 19.12 — Journey 8 (Ravi, custom profile builder) capture driver.

Walks PRD Journey 8's AUTHORING-AND-SHARING spine through the REAL binary: the
interactive wizard under a PTY, a hand-authored partial TOML whose `extends`
actually binds, `export`, the brand-new `profile install <local path>`, both
write-path refusals it now carries, and then — the beat that makes the whole
thing more than a filesystem exercise — ONE REAL AGENT TURN on the installed
profile.

The SHELL driver (`journey-J8-ravi.sh`) owns the stub lifecycle, the receipt
header and the trailers; this script owns the ten beats and the labelled fences.

# The Python half is MIXED, and that is a ruling not an accident (A12)

Beats 1–9 are CLI invocations: `RustainTUI` cannot run them (it blocks on a TUI
`Ready` wait, builds a pyte screen, copies `rustain/.env` and scaffolds a
permissions file), so they go through `_lib.cli_pane` / `_lib.cli_wizard_pane`.
Beat 10 is a real TUI turn and DOES use `RustainTUI`.

# Why beat 10 exists at all — the sweep forces it (A2)

`check-epic-close-gates.sh`'s committed-receipt sweep globs
`journeys/*/receipts/*.transcript.txt` unconditionally, and
`journey-J8-<stamp>.transcript.txt` matches `verify_receipt`'s
`journey-J[0-9]*-*` arm. That arm demands a product-minted ledger millisecond
(`"timestampMs":<13>`) and a `scene:` header whose file exists and hashes.
`UsageLedgerPort::append` has three production call sites — `turn.rs:301`,
`turn.rs:711`, `subagent_provider.rs:571` — and ZERO in `adapters/cli/`, so no
CLI verb can mint one. A capture without a turn would fail the sweep where
`GATE=""`, i.e. with a global failure and NO verdict line explaining it.

So the capture ends with PRD J8's Rising Action — "Investigate the OOM kills in
the billing service." — and the checker needs zero helper edits.

# The profile is selected by ENV, not by `--profile` (A2, corrected at preflight)

`RustainTUI.start()` builds `args = [binary] + (["--new"] if fresh)` and has no
`extra_args` parameter; `harness.py` is off-limits. So the door is
`env_overrides` carrying `RUSTAIN_PROFILE=devops`, honoured at
`profile_resolution.rs:17-23` (`--profile` flag -> `RUSTAIN_PROFILE` -> config
`active_profile` -> `coding`). ⛔ Do NOT add `extra_args` to the harness to make
a nicer sentence true.

# One config dir for both halves, or the turn silently runs on `coding` (A23c)

`harness.py:217` FORCES `RUSTAIN_CONFIG_DIR=<workspace>/.rustain`. If the CLI
beats installed into any other directory the TUI would never see `devops`, and
`startup.rs:296-322` turns `ProfileNotFound` into a `tracing::warn!` and falls
back to `coding` at exit 0 — marker 6 red with no diagnostic. So the CLI beats
use that exact path. ⛔ Do NOT instead override `RUSTAIN_CONFIG_DIR` in
`env_overrides`: the harness writes `.rustain/config.toml` carrying
`always_tools = []` at `config_dir()`, so moving it would silently disarm this
capture's own `allowed_tools=[]` precaution.

# Two profiles, two jobs (A23a)

The wizard creates `devops-wizard`; the shared fixture is `devops`. They are
different artifacts proving different things — authoring versus sharing. Naming
both `devops` (as the story originally did) made beat 5's install hit
`check_name_collision`'s user-profile arm, and because `import`'s overwrite
prompt has NO `is_terminal` guard it answered itself: "Import cancelled.
Existing profile preserved.", exit 0, the fixture never landing — and then
`profile show devops` read the WIZARD's file and marker 2 went green off the
wrong artifact.

# Evidence discipline

⛔ Nothing asserts on a string this script printed (the 19.9 HIGH). Every needle
`gate J8` greps is product-minted: `create.rs:307-312`'s completion line,
`import.rs:136-139`'s success line, `export.rs:71`'s, `install.rs:47-48`'s
gh-spec errors, `show.rs:88-91`'s padded adapter rows, `prompt.rs:59-63`'s
traversal refusal and `source.rs`'s collision refusal. ⛔ The turn's reply is
stub-served and is asserted NOWHERE; what is asserted is that the turn HAPPENED
under the installed profile.

Usage (normally invoked by journey-J8-ravi.sh):
    RUSTAIN_TUI_BINARY=<bin> python3 journey_j8_ravi.py <workspace> <stub-url> \
        <scratch-home> <run-dir>

Exit status: 0 only if every beat reached its pinned state and every pinned exit
code matched.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

RUSTAIN_SRC = Path(os.environ["RUSTAIN_TUI_BINARY"]).resolve().parents[2]
sys.path.insert(0, str(RUSTAIN_SRC / "tests_tui"))
sys.path.insert(0, str(RUSTAIN_SRC / "tests_tui" / "journeys"))

from _lib import (  # noqa: E402
    cli_pane,
    cli_wizard_pane,
    log,
    pane,
    quit_ctrl_q,
    require,
    sha256_of,
)
from harness import RustainTUI  # noqa: E402

HERE = Path(__file__).resolve().parent
FIXTURE = HERE / "profiles" / "devops.toml"

PERSONA = "scene-ravi"
BIN = Path(os.environ["RUSTAIN_TUI_BINARY"]).resolve()

# PRD Journey 8's Rising Action, verbatim enough to be recognisable and short
# enough to survive the stub's 120-character `last_user` clip.
RAVI_PROMPT = "Investigate the OOM kills in the billing service."
REPLY_NEEDLE = "binding constraint"

# The wizard's answer script, pinned at Task 0 T0.3(1) by driving the RELEASE
# binary. ⚑ Two prompts NEVER PRINT because `--extends coding` is supplied:
# `create.rs:90` gates the whole `Available parents:` block on
# `extends.is_none()` and `:105-110` skips `Extends (optional, leave blank for
# none): `. This is the script for the invocation the driver actually uses, not
# for the abstract one.
#
# ⚠ The seven `Available {port} adapters: {list}` lines are FEATURE-DEPENDENT.
# Measured against the release binary (`--features p2p,a2a`):
#   persona   : minimal, coding, personal-assistant
#   memory    : noop, project-scoped, daily-log
#   session   : basic, workspace
#   tools     : builtin-only, builtin-full, composite
#   channels  : terminal, telegram
#   scheduler : none, cron
#   context   : default, daily, composite
WIZARD_SCRIPT: list[tuple[str, str]] = [
    ("Description (optional): ", "Ravi's first pass, built in the wizard"),
    ("persona adapter: ", "coding"),
    ("memory adapter: ", "project-scoped"),
    ("session adapter: ", "workspace"),
    ("tools adapter: ", "composite"),
    ("channels adapter: ", "terminal"),
    ("scheduler adapter: ", "none"),
    ("context adapter: ", "default"),
    ("Add overrides? (e.g., default_plan_mode, model) [y/n] ", "n"),
    ("? [y/n] ", "y"),
]


def env_block(stub_url: str, home: Path) -> dict[str, str]:
    """The 19.7 A5 env block — J0's door, copied from `journey_j3_marco.py`.

    ⛔ Copied, not paraphrased. `ANTHROPIC_AUTH_TOKEN` empty is *unset*, and it
    must be: `harness.start()` copies `rustain/.env`, which exports a real
    bearer token, and the auth precedence is AUTH_TOKEN > API_KEY. `HOME` is
    redirected because the user-global config layer is read from
    `dirs::home_dir()` and a developer's `[provider.*]` there would take the
    CONFIG path and ignore the stub entirely.

    ⚑ `RUSTAIN_PROFILE` is the ONE addition J8 makes, and it is the whole point
    of beat 10 (A2).
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
        "RUSTAIN_PROFILE": "devops",
    }


def main() -> int:
    workspace, stub_url, home, run_dir = (
        Path(sys.argv[1]),
        sys.argv[2],
        Path(sys.argv[3]),
        Path(sys.argv[4]),
    )
    home.mkdir(parents=True, exist_ok=True)
    workspace.mkdir(parents=True, exist_ok=True)

    # THE config dir: exactly the path `harness.py:217` will force on the TUI in
    # beat 10. Both halves of this capture share it, or the turn falls back to
    # `coding` in silence (A23c).
    config_dir = workspace / ".rustain"
    config_dir.mkdir(parents=True, exist_ok=True)

    print(f"[witness] config dir shared by both halves: {config_dir}", flush=True)
    print(f"[witness] committed fixture sha256: {sha256_of(FIXTURE)}", flush=True)

    # ── Beat 1 — the non-TTY negative control ───────────────────────────────
    #
    # Assertable only because Story 19.12 A18 replaced `create.rs`'s `bail!`
    # with `eprintln!` + `exit(2)`. Before that this invocation produced ZERO
    # bytes on stdout AND stderr at exit 1 (measured twice), so an AC naming its
    # sentence would have been un-fireable and marker 1's control could only
    # have checked an exit status.
    log("beat 1 — `profile create` with no terminal must refuse OUT LOUD")
    cli_pane(
        BIN,
        ["profile", "create", "--name", "devops-wizard-notty"],
        label="create-no-tty",
        config_dir=config_dir,
        cwd=run_dir,
        expect_exit=2,
    )
    if (config_dir / "profiles" / "devops-wizard-notty.toml").exists():
        print("FAIL: the refused non-TTY create still wrote a profile", flush=True)
        return 1
    log("beat 1 — and it wrote nothing")

    # ── Beat 2 — the wizard, under a real PTY ───────────────────────────────
    #
    # ⛔ The wizard's profile is `devops-wizard`, NEVER `devops` (A23a).
    log("beat 2 — the interactive builder, driven under a PTY")
    cli_wizard_pane(
        BIN,
        ["profile", "create", "--name", "devops-wizard", "--extends", "coding"],
        WIZARD_SCRIPT,
        label="wizard",
        config_dir=config_dir,
        cwd=run_dir,
    )
    wizard_file = config_dir / "profiles" / "devops-wizard.toml"
    if not wizard_file.exists():
        print(f"FAIL: the wizard said it wrote {wizard_file} and did not", flush=True)
        return 1
    sections = sum(1 for line in wizard_file.read_text().splitlines() if line.startswith("["))
    print(f"[witness] wizard wrote {wizard_file}", flush=True)
    print(f"[witness] wizard profile port sections: {sections}", flush=True)
    if sections != 7:
        print(f"FAIL: the wizard wrote {sections} port sections, not 7", flush=True)
        return 1

    # ── Beat 3 — export the wizard's profile (marker 5) ─────────────────────
    #
    # ⚠ Sections come out ALPHABETICALLY, not in `PORT_ORDER`, because
    # `to_flat_toml` builds a `toml::Table` — despite the comment at
    # `profile_serializer.rs:86` saying "canonical order". ⛔ No marker literal
    # may assume `PORT_ORDER`. Header line 3 is `chrono::Utc::now()`, so the
    # artifact is not byte-stable and nothing hashes it.
    log("beat 3 — export flattens the extends chain into one shareable file")
    exported = run_dir / "devops-wizard.toml"
    export_before = {path for path in run_dir.rglob("*") if path.is_file()}
    cli_pane(
        BIN,
        ["profile", "export", "devops-wizard", "-o", str(exported)],
        label="export-wizard",
        config_dir=config_dir,
        cwd=run_dir,
        expect_exit=0,
    )
    export_after = {path for path in run_dir.rglob("*") if path.is_file()}
    export_created = sorted(export_after - export_before)
    print("--- pane: export-created-artifacts ---", flush=True)
    for path in export_created:
        print(f"<run>/{path.relative_to(run_dir)}", flush=True)
    print("--- end pane ---", flush=True)
    if not exported.exists():
        print(f"FAIL: export claimed {exported} and wrote nothing", flush=True)
        return 1
    print("--- pane: export-artifact ---", flush=True)
    print(exported.read_text(), flush=True)
    print("--- end pane ---", flush=True)

    # ── Beat 4 — SETUP, not narrative (A23b) ────────────────────────────────
    #
    # Beat 6 needs a file whose in-file `name` is `coding`, and `coding` is
    # built-in: there is no `coding.toml` on disk in a fresh config dir, and the
    # ONLY producer in the tree is `profile export coding`. Without this beat,
    # beat 6 hits the missing-file path, which after A6's delegation is an
    # `anyhow::Err` — exit 1, zero bytes on both streams, and marker 4's needle
    # simply absent with nothing saying why.
    log("beat 4 — export `coding` purely to give beat 6 a file to point at")
    coding_toml = run_dir / "coding.toml"
    cli_pane(
        BIN,
        ["profile", "export", "coding", "-o", str(coding_toml)],
        label="export-coding-setup",
        config_dir=config_dir,
        cwd=run_dir,
        expect_exit=0,
    )

    # ── Beat 5 — THE VERB THIS STORY IS NAMED FOR ───────────────────────────
    #
    # ⚠ `devops` is installed EXACTLY ONCE. With A21's guard a second
    # `install ./devops.toml` hits `check_name_collision`'s user-profile arm
    # against the very directory it writes to and is refused, where `import`
    # would prompt — the idempotency inversion in A22.
    log("beat 5 — `profile install <local path>`: the shared artifact moves in")
    cli_pane(
        BIN,
        ["profile", "install", str(FIXTURE)],
        label="install-local-path",
        config_dir=config_dir,
        cwd=run_dir,
        expect_exit=0,
    )
    installed = config_dir / "profiles" / "devops.toml"
    print(f"[witness] installed artifact path: {installed}", flush=True)
    print(f"[witness] installed artifact exists: {installed.exists()}", flush=True)
    if not installed.exists():
        print(f"FAIL: install did not land the profile at {installed}", flush=True)
        return 1
    # ⛔ The gate reads the PATH, never `profile show`'s `Source:` line:
    # `show.rs:106-117` returns `User` for any non-embedded name, so `Source:`
    # can never print `community` and a marker reading it would be meaningless
    # (A14/A22).
    print("--- pane: install-destination ---", flush=True)
    print(f"landed: {installed}", flush=True)
    print(f"community sidecar present: {(config_dir / 'profiles' / 'community').exists()}", flush=True)
    print("--- end pane ---", flush=True)

    # ── Beat 6 — the built-in collision refusal (A21) ───────────────────────
    log("beat 6 — installing someone else's `coding` must be refused")
    cli_pane(
        BIN,
        ["profile", "install", str(coding_toml)],
        label="refuse-collision",
        config_dir=config_dir,
        cwd=run_dir,
        expect_exit=2,
    )

    # ── Beat 7 — the traversal refusal, on BOTH verbs (A20) ─────────────────
    #
    # ⚠ The file NEEDS `extends`: a bare `name = …` fails validation first
    # ("missing required dimensions", exit 2) and a dev following the short
    # recipe would see a refusal and conclude the finding was wrong.
    log("beat 7 — a file whose own name escapes the config dir must be refused")
    evil = run_dir / "evil.toml"
    evil.write_text('name = "../../ESCAPED"\nextends = "coding"\n')
    cli_pane(
        BIN,
        ["profile", "install", str(evil)],
        label="refuse-traversal-install",
        config_dir=config_dir,
        cwd=run_dir,
        expect_exit=2,
    )
    cli_pane(
        BIN,
        ["profile", "import", str(evil)],
        label="refuse-traversal-import",
        config_dir=config_dir,
        cwd=run_dir,
        expect_exit=2,
    )
    # ⚑ Where the escape LANDS, measured at Task 0: `dest` is
    # `<config_dir>/profiles/../../ESCAPED.toml`, which the OS resolves to
    # `<config_dir>/../ESCAPED.toml` — one level ABOVE the config dir, i.e. the
    # workspace root here. Getting this path wrong is how the check passes
    # vacuously while the write still happens.
    escaped = config_dir.parent / "ESCAPED.toml"
    print(f"[witness] escaped write target: {escaped}", flush=True)
    print(f"[witness] escaped write present: {escaped.exists()}", flush=True)
    if escaped.exists():
        print(f"FAIL: a profile was written outside the config dir at {escaped}", flush=True)
        return 1

    # ── Beat 8 — the `gh:` route is untouched (A6) ──────────────────────────
    #
    # The discriminator is PRESENCE, not wording: branching on `parse_gh_spec`'s
    # error VARIANT instead of the spec prefix would send `gh:` down the
    # delegated path, where a missing file is an `anyhow::Err` — exit 1, zero
    # bytes on both streams. So this beat is what keeps A6's predicate honest.
    log("beat 8 — a `gh:` spec still prints its own gh-spec error on stderr")
    cli_pane(
        BIN,
        ["profile", "install", "gh:"],
        label="gh-spec-untouched",
        config_dir=config_dir,
        cwd=run_dir,
        expect_exit=2,
    )

    # ── Beat 9 — `extends` BINDS (marker 2) ─────────────────────────────────
    #
    # ⛔ Marker 2's pane is THIS one, never the wizard's: pexpect echoes the
    # answers the driver typed — `coding`, `project-scoped` — into the wizard's
    # own fence, and those are the same words. This pane carries
    # `show.rs:88-91`'s padded `{label:<12}` column, which a typed answer cannot
    # produce (A23d).
    log("beat 9 — `profile show devops`: five rows the fixture never names")
    cli_pane(
        BIN,
        ["profile", "show", "devops"],
        label="show-installed",
        config_dir=config_dir,
        cwd=run_dir,
        expect_exit=0,
    )

    # ── Beat 10 — one real turn on the installed profile ────────────────────
    log("beat 10 — one real agent turn, resolved from RUSTAIN_PROFILE=devops")
    tui = RustainTUI(
        fresh=True,
        build=False,
        workspace=workspace,
        allowed_tools=[],  # nothing pre-allowed
        env_overrides=env_block(stub_url, home),
        timeout=60,
    ).start()
    # The harness copied `rustain/.env` — a real credential one `Read` away from
    # a committed pane dump — into the workspace.
    (workspace / ".env").unlink(missing_ok=True)
    require(tui, "Ready", 30, "boot on the installed profile")
    pane(tui, "boot-devops")
    tui.send_message(RAVI_PROMPT)
    require(tui, REPLY_NEEDLE, 60, "the turn completed")
    pane(tui, "ravi-turn")
    quit_ctrl_q(tui)

    print("[beats] complete", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
