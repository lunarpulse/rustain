#!/usr/bin/env python3
"""Story 19.11 — Journey 3 (Marco, skill creator) capture driver, pexpect half.

Walks PRD Journey 3 through the REAL binary under a PTY against the 19.7 scene
stub: Marco's own `.agents/skills/safe-deploy/SKILL.md` — the PRD's bytes, byte
for byte — discovered at boot, activated by a model-driven `activate_skill`,
trust-gated, read to the model as a `<skill>` block, and enforced. On the second
submission the pattern-restricted Bash tool is offered; hermetic `helm` and
`kubectl` commands execute after real approval prompts, while a blocklist-clean
undeclared chain is denied.

The SHELL driver (`journey-J3-marco.sh`) owns the stub lifecycle, the receipt
header and the three trailers; this script owns the keystrokes, the screen-state
waits and the labelled pane dumps.

Front door: J0's ENV door (`ANTHROPIC_BASE_URL` + `ANTHROPIC_API_KEY`, no
`[provider.*]` at all), because Marco's junior has a rustain and a key and
nothing else. ⛔ Activation enters through `execute_activate_skill`
(`toolset_adapter.rs:1604`) from a real streamed `tool_use` block — never
through `submit_slash(t, "/safe-deploy")`, which is the user-driven route
(`event_loop.rs:7733`) and explicitly not the PRD's climax.

Why the capture takes TWO user submissions (ruling A14 — the preflight's
headline): `turn_driver::submit` computes the offered catalogue (`:420`), the
system prompt (`:425-431`) and the FR42-a disclosure (`:451-524`) ONCE, before
`tokio::spawn(turn::run_turn)` at `:639`, and `turn.rs:216` clones `options`
unchanged on every loop iteration. A one-message scene would therefore POST the
PRE-activation catalogue four times, carry no `<skill>` block and mint zero
Advisories. Submission 1 activates; submission 2 is where the restriction bites.

Same-turn execution enforcement is now the load-bearing regression contract.
The scene scripts a hermetic excluded `Bash` immediately after
`activate_skill`; the runtime refreshes the live activation set, returns a
skill-policy denial before risk approval, and never emits command stdout.
Marker 6 requires both denial delivery and stdout absence. The driver tolerates
the historical executed result only so the executing gate—not a timeout—
reports the regression; see `midturn_probe`.

Evidence discipline (19.7 A4 / 18-4e A16 / this story's A2): the screen is a
witness. The exception J3 makes is that its most important screen strings are
PRODUCT-MINTED — the trust prompt (`skill_trust_prompt.rs:31-61`), the FR42-a
disclosure (`turn_driver.rs:493-512`), both `activate_skill` results
(`toolset_adapter.rs:1643`/`:1652`) — and the stub cannot make the binary say
them. ⛔ Nothing asserts on a string THIS SCRIPT printed (the 19.9 HIGH).

Usage (normally invoked by journey-J3-marco.sh):
    RUSTAIN_TUI_BINARY=<bin> python3 journey_j3_marco.py \
        <workspace> <stub-url> <scratch-home> <decline-workspace> <bare-repo>

Exit status: 0 only if every beat reached its pinned screen state, the committed
fixture matches the PRD-pinned digest, and its workspace copy matches the fixture.
"""

from __future__ import annotations

import os
import subprocess
import sys
import time
from pathlib import Path

RUSTAIN_SRC = Path(os.environ["RUSTAIN_TUI_BINARY"]).resolve().parents[2]
sys.path.insert(0, str(RUSTAIN_SRC / "tests_tui"))
sys.path.insert(0, str(RUSTAIN_SRC / "tests_tui" / "journeys"))

from _lib import (  # noqa: E402
    log,
    pane,
    quit_ctrl_q,
    require,
    require_gone,
    sha256_of,
)
from fixtures.skills import write_workspace_skill  # noqa: E402
from harness import RustainTUI  # noqa: E402
from keys import CTRL_X, ENTER, ESC, TAB  # noqa: E402

HERE = Path(__file__).resolve().parent
SKILL_SRC = HERE / "skills" / "safe-deploy" / "SKILL.md"
PRD_SKILL_SHA256 = "a12d9bf49cfa431ee4e3a5b09e16300a535d064a8cfc0c411773ebfd6b8a77d5"

PERSONA = "scene-marco"
TEAM_PERSONA = "scene-marco-team"
DECLINE_PERSONA = "scene-marco-decline"

# The two files Marco's protocol reads. Short plain ASCII (19.9 A16: <= 6 lines
# of <= 60 chars) with the needle on line 1, because `Read` returns
# "<n>\t<line>" rows and the stub clips `last_user` at 120 characters — a needle
# on line 4 would be invisible to the marker that proves the read round-tripped.
# ⛔ Neither needle appears in `scenes/j3-marco-v2.json`; `gate J3` checks that
# precondition before it runs anything.
DEPLOY_YAML = (
    "# J3-MARCO-DEPLOY-NEEDLE billing prod\n"
    "service: billing\n"
    "environment: production\n"
    "chart: charts/billing\n"
    "replicas: 3\n"
)
MIGRATION_SQL = (
    "-- J3-MARCO-MIGRATION-NEEDLE 0042\n"
    "ALTER TABLE billing DROP COLUMN legacy_id;\n"
    "-- breaking: legacy_id is still read by v1\n"
)

# The five runbooks Marco pushes to the team repo (PRD Resolution). Each is a
# minimal VALID skill: `skill_registry.rs:372-392` drops one that is missing
# `description:`, and a dropped skill would silently move the discovery count
# this capture pins.
#
# ⚑ The names are SHORT on purpose. The product's own catalogue read-back —
# `Skill not found: audit. Discovered skills: [...]`
# (`toolset_adapter.rs:1656-1666`) — is the surface that proves the count rose,
# and it proves it on the WIRE, where the stub clips `last_user` at 120
# characters. All six names plus the prefix fit in 117; longer names would
# truncate the list and turn a count into a guess.
TEAM_SKILLS = {
    "canary-watch": "Watch canary error budgets during a staged rollout.",
    "db-guard": "Refuse destructive migrations without a written plan.",
    "oncall-drill": "Run the on-call incident drill end to end.",
    "rollback": "Roll a blue/green deploy back to the previous colour.",
    "smoke-suite": "Run the post-deploy smoke suite and report failures.",
}

# The mid-turn probe's needle (A15). It is COMPOSED BY `printf` at run time from
# two scene fragments (`J3-MARCO-` and `MIDTURN`), so the joined literal is not a
# byte of `j3-marco-v2.json` and the gate's echo precondition is executable.
MIDTURN_NEEDLE = "J3-MARCO-MIDTURN"
HELM_NEEDLE = "J3-MARCO-HELM-EXECUTED"
KUBECTL_NEEDLE = "J3-MARCO-KUBECTL-EXECUTED"
CHAIN_NEEDLE = "J3-MARCO-CHAIN-SECOND-RAN"
TEAM_CONTROL_SKILL = "canary-watch"


def env_block(stub_url: str, home: Path, persona: str, shim_dir: Path) -> dict[str, str]:
    """The 19.7 A5 env block — J0's door, copied from `SceneStub.env` in effect.

    `ANTHROPIC_AUTH_TOKEN` empty is *unset*, and it must be: `harness.start()`
    copies `rustain/.env`, which exports a real bearer token, and the auth
    precedence is AUTH_TOKEN > API_KEY. `HOME` is redirected because the
    user-global config layer is read from `dirs::home_dir()`, a developer's
    `[provider.*]` there would take the CONFIG path and ignore the stub, and
    `SkillRegistry::discover` also scans `<home>/.agents/skills` — a developer's
    global skills would move the discovery count this capture pins.
    """
    return {
        "ANTHROPIC_AUTH_TOKEN": "",
        "ANTHROPIC_API_KEY": persona,
        "ANTHROPIC_BASE_URL": stub_url,
        "ANTHROPIC_DEFAULT_SONNET_MODEL": "",
        "RUSTAIN_MODELS_DEV_URL": stub_url,
        "OPENROUTER_API_KEY": "",
        "HOME": str(home),
        "NO_COLOR": "1",
        "PATH": f"{shim_dir}{os.pathsep}{os.environ.get('PATH', '')}",
    }


def launch(
    workspace: Path,
    stub_url: str,
    home: Path,
    persona: str,
    shim_dir: Path,
    *,
    fresh: bool,
) -> RustainTUI:
    tui = RustainTUI(
        fresh=fresh,
        build=False,
        workspace=workspace,
        allowed_tools=[],  # A8 — nothing pre-allowed
        env_overrides=env_block(stub_url, home, persona, shim_dir),
        timeout=60,
    ).start()
    # A8 / 19.9 preflight: the harness copied `rustain/.env` — a real credential
    # one `Read` away from a committed pane dump — into the workspace.
    (workspace / ".env").unlink(missing_ok=True)
    return tui


def install_skill(workspace: Path) -> str:
    """Verify the PRD-pinned fixture, copy it, and compare the destination."""
    src_sha = sha256_of(SKILL_SRC)
    print(f"[witness] committed SKILL.md sha256: {src_sha}", flush=True)
    if src_sha != PRD_SKILL_SHA256:
        print(
            "FAIL: the committed SKILL.md is not the PRD-pinned bytes — "
            f"{src_sha} != {PRD_SKILL_SHA256}",
            flush=True,
        )
        sys.exit(1)

    written = write_workspace_skill(workspace, SKILL_SRC)
    dst_sha = sha256_of(written)
    print(f"[witness] workspace SKILL.md sha256: {dst_sha}", flush=True)
    if src_sha != dst_sha:
        print(
            "FAIL: the workspace copy is not the committed PRD bytes — "
            f"{dst_sha} != {src_sha}",
            flush=True,
        )
        sys.exit(1)
    print(f"[witness] the binary will load the PRD's own bytes ({src_sha})", flush=True)
    return src_sha

def install_command_shims(scratch: Path) -> Path:
    """Install hermetic executables; each needle is the first output line."""
    shim_dir = scratch / "command-shims"
    shim_dir.mkdir(parents=True, exist_ok=True)
    for command, needle in [("helm", HELM_NEEDLE), ("kubectl", KUBECTL_NEEDLE)]:
        shim = shim_dir / command
        shim.write_text(f"#!/bin/sh\nprintf '%s\\n' '{needle}'\n")
        shim.chmod(0o755)
    return shim_dir


def seed_team_repo(bare: Path, scratch: Path) -> None:
    """Build the team's shared skills repo — a real local bare git repository.

    PRD J3's Resolution is *"Marco creates 5 more skills and pushes them to the
    team's shared git repo"*. A real `git init --bare` + `git push` here is what
    makes the clone in `clone_team_skills` a real clone rather than a `cp -r`
    wearing git's name.
    """
    bare.mkdir(parents=True, exist_ok=True)
    _git(["init", "--bare", "--initial-branch=main", str(bare)], cwd=bare.parent)
    seed = scratch / "team-skills-seed"
    seed.mkdir(parents=True, exist_ok=True)
    _git(["init", "--initial-branch=main"], cwd=seed)
    _git(["config", "user.email", "marco@example.invalid"], cwd=seed)
    _git(["config", "user.name", "Marco"], cwd=seed)
    for name, description in TEAM_SKILLS.items():
        skill_dir = seed / name
        skill_dir.mkdir(parents=True, exist_ok=True)
        allowed_tools = (
            "allowed-tools: Read Glob\n" if name == TEAM_CONTROL_SKILL else ""
        )
        (skill_dir / "SKILL.md").write_text(
            f"---\nname: {name}\ndescription: {description}\n"
            f"{allowed_tools}---\n"
            f"## Protocol\n1. Follow the team runbook for {name}.\n"
        )
    _git(["add", "."], cwd=seed)
    _git(["commit", "-m", "five more runbooks"], cwd=seed)
    _git(["push", str(bare), "main"], cwd=seed)


def clone_team_skills(bare: Path, workspace: Path, scratch: Path) -> list[str]:
    """`git clone` the team repo and drop its runbooks into `.agents/skills/`."""
    checkout = scratch / "team-skills-clone"
    _git(["clone", str(bare), str(checkout)], cwd=scratch)
    installed = []
    for entry in sorted(checkout.iterdir()):
        if entry.name == ".git" or not entry.is_dir():
            continue
        target = workspace / ".agents" / "skills" / entry.name
        target.mkdir(parents=True, exist_ok=True)
        (target / "SKILL.md").write_text((entry / "SKILL.md").read_text())
        installed.append(entry.name)
    log(f"cloned {len(installed)} team runbooks into .agents/skills: {installed}")
    return installed


def _git(args: list[str], cwd: Path) -> None:
    result = subprocess.run(  # noqa: S603 - fixed argv, no shell
        ["git", *args],
        cwd=str(cwd),
        capture_output=True,
        text=True,
        env={**os.environ, "GIT_TERMINAL_PROMPT": "0"},
    )
    if result.returncode != 0:
        print(
            f"FAIL: git {' '.join(args)} exited {result.returncode}: {result.stderr}",
            flush=True,
        )
        sys.exit(1)


def midturn_probe(tui) -> None:
    """Run the A15 probe immediately after model-driven skill activation.

    The driver deliberately accepts both historical and fixed behavior; the
    executing gate distinguishes them:

    * The pre-review stale snapshot let `Bash` reach the Elevated risk prompt.
      Approval then executed the command, and stdout reached the next request.
    * The fixed runtime refreshes live skill state after activation.
      `permission_chain` denies `Bash` before risk approval, the model receives
      `Tool 'Bash' not allowed by skill 'safe-deploy'…`, and command stdout is
      absent.

    A strict prompt wait would turn the fix into a driver timeout instead of
    letting marker 6 prove it. The scene's next `expect` is `Tool 'Bash'`, a
    substring of both result shapes, so one scene detects either behavior.

    Returns after dumping `midturn-bash`, whether or not a prompt appeared.
    """
    prompted = tui.wait_for_screen("[y] Allow", timeout=45)
    pane(tui, "midturn-bash")
    if prompted:
        log("mid-turn Bash reached the risk gate and raised a prompt (stale snapshot)")
        tui.send("y")
    else:
        log("no permission prompt for the mid-turn Bash — it never reached the risk gate")


def unfold_finished_turn(tui) -> None:
    """Open the finished submission-1 turn and its first tool block.

    ⚑ Three T0.3 measurements shape this, and the third is a finding.

    1. Submission 1's turn carries FOUR invocations, and
       `chat_pane::default_collapse_predicate` folds a completed turn with
       >= 3 tools whose tool lines outweigh its prose into a single
       `▸ … · 4 tools, 2.9s ✓` row. `zR` (Story 16.6 `ExpandAllTurns`) is what
       reopens it — `view_state.collapsed[turn] = false` beats the predicate in
       `effective_is_collapsed`.
    2. With the turn open, `Enter` toggles `state.focused_tool_id`, which the
       renderer seats on the turn's FIRST visible tool block — invocation 0,
       the `activate_skill` call. That is how `Skill 'safe-deploy' activated.`
       — the product's own tool result, the string marker 2 asserts on the wire
       — is put on screen as a witness.
    3. ⛔ **The other three blocks are NOT reachable by keyboard in this
       conversation, and that is a product gap, not a driver shortcut.** `Tab`
       (`CycleInvocationInFocusedTurn`, `event_loop.rs:5153-5162`) is guarded on
       `view_state.focused_turn` being `Some`, and the only thing that seats it
       is `]]`/`[[` — whose `start_ref` is `focused.or(topmost_on_screen)`
       (`event_loop.rs:4909`). In a conversation with exactly ONE assistant
       turn, that turn IS `topmost_on_screen`, so `]]` searches strictly after
       it and `[[` strictly before it; both find nothing, both flash "No next
       prose turn", and `focused_turn` stays `None`. Measured, not inferred: a
       diagnostic run pressed `g ]] Tab Tab Enter` and opened invocation 0.
       J0's honesty block claims "every block is expandable by keyboard as of
       this tree"; that claim has this hole.
       `DF-19-10-SINGLE-TURN-FOCUS-UNREACHABLE`, filed, not fixed here (A1: no
       `src` in a capture story).

    So `deploy-read` and `migration-read` witness their blocks as the rendered
    rows of the opened turn rather than as expanded bodies. The READ evidence
    that matters is not on screen anyway: it is the driver-written needle
    round-tripping into the next request's `last_user` (marker 5).
    """
    tui.send(ESC)
    time.sleep(0.5)
    tui.send("z")
    tui.send("R")
    time.sleep(0.8)
    tui.send(ENTER)
    time.sleep(1.2)


def main() -> int:
    workspace = Path(sys.argv[1])
    stub_url = sys.argv[2]
    home = Path(sys.argv[3])
    decline_workspace = Path(sys.argv[4])
    scratch = Path(sys.argv[5])

    workspace.mkdir(parents=True, exist_ok=True)
    decline_workspace.mkdir(parents=True, exist_ok=True)
    home.mkdir(parents=True, exist_ok=True)
    scratch.mkdir(parents=True, exist_ok=True)

    # ── Marco's workspace: his runbook-as-skill and the files it reads ───────
    install_skill(workspace)
    install_skill(decline_workspace)
    (workspace / "deploy.yaml").write_text(DEPLOY_YAML)
    (workspace / "migrations").mkdir(parents=True, exist_ok=True)
    (workspace / "migrations" / "0042_drop_column.sql").write_text(MIGRATION_SQL)
    seed_team_repo(scratch / "team-skills.git", scratch)
    shim_dir = install_command_shims(scratch)

    # ── Launch 1 — the junior's rustain ──────────────────────────────────────
    tui = launch(workspace, stub_url, home, PERSONA, shim_dir, fresh=True)
    require(tui, "Ready", 30, "boot")
    # The `Loaded N skills` notice is a StatusFlash and expires; the status bar's
    # `Skills: N active` counts ACTIVE skills, which is 0 here. The persistent
    # screen state that proves DISCOVERY ran is the slash popup: skills are
    # exposed as slash commands, and `refresh_*_suggestions` cannot list one the
    # registry never found.
    for c in "/safe":
        tui.send(c)
        time.sleep(0.06)
    require(tui, "/safe-deploy", 20, "the discovered skill is listed by the slash popup")
    pane(tui, "boot-skills")
    tui.send(ESC)
    time.sleep(0.4)
    for _ in range(len("/safe")):
        tui.send("\x7f")
        time.sleep(0.05)

    # ── SUBMISSION 1 — "deploy billing to production" ────────────────────────
    tui.send_message("deploy billing to production")

    # The trust gate. Product-minted, all three lines (`skill_trust_prompt.rs`).
    require(
        tui,
        "Trust and enable this skill for this session?",
        60,
        "the first activation of a workspace skill is trust-gated",
    )
    pane(tui, "trust-prompt")  # + `New project skill detected:` + `[i] Inspect`

    # `[i]` — tier 2 for the OPERATOR. ⚠ This is not evidence the body reached
    # the MODEL; that claim is the request log's `skills` field (A7).
    tui.send("i")
    require(tui, "Inspect skill:", 20, "the inspect overlay opened")
    pane(tui, "trust-inspect")
    tui.send(ESC)
    require(
        tui,
        "Trust and enable this skill for this session?",
        15,
        "Esc returned to the trust prompt rather than declining",
    )

    tui.send("y")
    require_gone(tui, "Trust and enable", 20, "the trust prompt closed on `y`")

    # ⚑ A15 — the SAME turn, after `activate_skill` returned, requests a tool
    # excluded by the skill's `allowed-tools`. The fixed runtime refreshes the
    # live activation set before this call reaches the scheduler, so no risk
    # prompt paints and the binary returns a skill-policy denial.
    midturn_probe(tui)

    # The turn finishes: Protocol step 1's `Read deploy.yaml` and step 3's read
    # of the named migration both run. `Read` is `ToolRisk::Safe`
    # (`permission.rs:37-39`) and `permission_chain.rs:59` allows Safe
    # regardless of mode, so NO prompt paints for either — J3 is the first
    # capture with no approval gate on its reads (A8).
    require(tui, "Breaking migration detected", 60, "the stop-and-warn reply")

    # …and only now are the blocks photographed, from the finished turn.
    unfold_finished_turn(tui)
    pane(tui, "skill-active")  # invocation 0 expanded: `Skill 'safe-deploy' activated.`
    tui.send(ENTER)  # re-collapse; the four block headers stay on one screen
    time.sleep(0.8)
    pane(tui, "deploy-read")  # `Success Read "deploy.yaml"`
    pane(tui, "migration-read")  # `Success Read "migrations/0042_drop_column.sql"`
    tui.send("i")  # back to Input focus for submission 2
    time.sleep(0.5)

    # Keep the original Monitor workaround for a non-vacuous negative: if
    # safe-deploy were still disclosed, its Advisory would render in the first
    # permission pane instead of disappearing into Focus mode's queue.
    tui.send(CTRL_X)
    time.sleep(0.5)
    tui.send("w")
    require(tui, "Density: Monitor", 20, "submission 2 switched to Monitor density")
    tui.send(TAB)
    time.sleep(0.5)

    # ── SUBMISSION 2 — the pattern restriction admits only declared commands ─
    tui.send_message("preview the staging release")
    pane(tui, "sub2-sent")

    # Bash remains Elevated in Normal mode. Nothing is pre-approved, so each
    # admitted call reaches the product's real approval card and receives a
    # one-shot `y`; the later call must ask again.
    require(tui, "helm diff release charts/billing", 60, "helm reached approval")
    require(tui, "[y] Allow", 20, "the helm command raised the permission prompt")
    pane(tui, "helm-permission")
    tui.send("y")

    require(tui, "kubectl get pods", 60, "kubectl reached approval")
    require(tui, "[y] Allow", 20, "the kubectl command raised a fresh permission prompt")
    pane(tui, "kubectl-permission")
    tui.send("y")

    # The scene next dispatches `kubectl … && printf …`. Its first segment is
    # declared and blocklist-clean; the second is not. The chain must be denied
    # before either segment runs, then the scene serves the final reply only
    # after receiving that product-minted denial.
    require(tui, "Standing by on the preview step.", 60, "the denied chain returned")
    pane(tui, "post-pattern-execution")

    quit_ctrl_q(tui)
    log("launch 1 quit via Ctrl+Q")

    # ── The Resolution beat — a real clone of the team's shared repo ─────────
    print("\n=== the team clones Marco's runbooks (PRD J3 Resolution)", flush=True)
    clone_team_skills(scratch / "team-skills.git", workspace, scratch)

    # ── Launch 2 — a new conversation in a workspace that now has six ────────
    tui = launch(workspace, stub_url, home, TEAM_PERSONA, shim_dir, fresh=True)
    require(tui, "Ready", 30, "boot after the clone")
    tui.send_message("which runbooks are on this machine")
    # Two beats, one wait. The scene's first turn asks for a skill that does not
    # exist, so `activate_skill` answers with the product's own catalogue
    # read-back — `Skill not found: audit. Discovered skills: [...]`
    # (`toolset_adapter.rs:1656-1666`) — which rides the NEXT request's
    # `last_user` and is where the risen count is asserted (marker 1). ⚑ That
    # result is inside a COLLAPSED tool block, so it is not a screen state to
    # wait on (measured at T0.3); the scene's next turn asks for `safe-deploy`
    # again, and the wait is the product-minted prompt that answer produces.
    #
    # …which is itself the finding: trust was granted for this skill, in this
    # session, in launch 1. `skill_activation.rs:261-276` keys the decision on
    # `conversation_id`, so a new conversation asks again while the prompt says
    # "for this session" — `DF-19-10-TRUST-PROMPT-SAYS-SESSION`, demonstrated
    # rather than asserted.
    require(
        tui,
        "Trust and enable this skill for this session?",
        90,
        "a new conversation re-asks for trust the operator already granted",
    )
    pane(tui, "after-clone")
    tui.send("y")

    # The team persona immediately activates the cloned control runbook. It has
    # `Read Glob`, so its raw intersection with safe-deploy retains `Read` and
    # reaches the Advisory branch rather than the turn-fatal disjoint branch.
    require(tui, TEAM_CONTROL_SKILL, 60, "the team control requested activation")
    require(
        tui,
        "Trust and enable this skill for this session?",
        20,
        "the cloned control runbook raised its own trust prompt",
    )
    pane(tui, "team-control-trust")
    tui.send("y")
    require(tui, "The team runbooks are in place.", 60, "both runbooks activated")

    # DF-19-10-ADVISORY-QUEUED-IN-FOCUS: Monitor is still required to surface
    # the positive FR42-a control. The unavailable name is driver-owned `Glob`,
    # not one of the now-honourable Bash patterns.
    tui.send(CTRL_X)
    time.sleep(0.5)
    tui.send("w")
    require(tui, "Density: Monitor", 20, "the control switched to Monitor density")
    tui.send(TAB)
    time.sleep(0.5)
    tui.send_message("exercise the team runbook disclosure control")
    require(tui, "Glob", 60, "the unavailable team-runbook tool was disclosed")
    pane(tui, "team-control-disclosure")
    require(tui, "Team control complete.", 60, "the control turn replied")
    quit_ctrl_q(tui)
    log("launch 2 quit via Ctrl+Q")

    # ── Launch 3 — the DECLINE leg (A6's discriminating control) ─────────────
    # Without it the ordering claim is vacuous: a skill can only enter the
    # activation set through `activate()`, so "no request names the skill before
    # the activation result" is green on a build with NO trust gate at all.
    # Answering `n` is what makes the gate's presence observable.
    print("\n=== the decline leg — the same skill, answered `n`", flush=True)
    tui = launch(decline_workspace, stub_url, home, DECLINE_PERSONA, shim_dir, fresh=True)
    require(tui, "Ready", 30, "boot (decline leg)")
    tui.send_message("deploy billing to production")
    require(
        tui,
        "Trust and enable this skill for this session?",
        60,
        "the decline leg reached the same trust gate",
    )
    pane(tui, "decline-prompt")
    tui.send("n")
    # ⚑ The decline result is a TOOL RESULT in a collapsed block, not a
    # SystemNotice: the `Skill '…' not trusted — activation declined` notice at
    # `event_loop.rs:7783` belongs to the USER-driven route. On the model-driven
    # route the string reaches the MODEL, and that is where marker 2 asserts it
    # — in the decline leg's own request-log rows.
    require_gone(tui, "Trust and enable", 20, "the trust prompt closed on `n`")
    require(tui, "Understood.", 60, "the decline leg's turn completed")
    pane(tui, "decline-refused")
    quit_ctrl_q(tui)
    log("decline leg quit via Ctrl+Q")

    print("[beats] complete", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
