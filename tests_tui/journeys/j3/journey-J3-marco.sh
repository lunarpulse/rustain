#!/usr/bin/env bash
#
# Epic 19 journey gate J3 — Marco's runbook-as-skill, enforced by the real binary
# (Story 19.10): the PRD's own `.agents/skills/safe-deploy/SKILL.md`, byte for
# byte, discovered at boot, activated by a model-driven `activate_skill`,
# TRUST-GATED with `[y]`/`[n]`/`[i]`, read to the model as a `<skill>` block, and
# then — on a SECOND user submission — enforced: the offered catalogue shrinks,
# the operator is told in the product's own words which restriction this build
# cannot honour, and the turn survives being told. Asserted by
# `check-epic-close-gates.sh`'s `gate J3`.
#
# # Why the capture takes TWO user submissions
#
# `turn_driver::submit` computes the offered catalogue, the system prompt and
# the FR42-a disclosure ONCE, before `tokio::spawn(run_turn)`, and `turn.rs`
# clones the options unchanged every loop iteration. A one-message scene POSTs
# the PRE-activation catalogue on every round trip, carries no `<skill>` block
# and mints zero Advisories — so submission 1 activates, and submission 2 is
# where the restriction bites (story ruling A14).
#
# # Same-turn enforcement is an executing assertion
#
# After successful `activate_skill`, `run_turn` refreshes the conversation's
# live activation set before scheduling the next tool call. The scene requests
# a hermetic excluded `Bash` immediately after activation. Marker 6 requires
# both the binary-generated denial result and absence of the command's stdout,
# so deleting the probe or regressing the refresh turns the gate red.
#
# # What this driver owns, and what it does not
#
# This shell owns the stub lifecycle, the receipt header (the `tree:` line that
# names the binary's source tree, the `scene:` hash that makes the capture
# replayable and the `skill:` hash that names the bytes the binary loaded), the
# pexpect walk (`journey_j3_marco.py`) and the three receipt-integrity trailers
# — the usage ledger (product-minted milliseconds), `session list --json` and
# the stub's request log. The SCREEN is a witness — except where the screen IS
# the product's own sentence: the trust prompt, the FR42-a disclosure and both
# `activate_skill` results are strings the stub cannot make the binary say.
#
# ⚑ The `skill:` hash is a reading aid HERE and an ASSERTION inside the pexpect
# half: `journey_j3_marco.py` first compares the committed fixture to the
# PRD-pinned digest, then compares the workspace copy to the fixture. Either
# mismatch fails the run.
#
# # Honest limits, printed in the receipt's final block
#
# See the `LIMITS` heredoc below; every one of them is a reading aid and no gate
# greps any of it.
#
# # Aborted runs leave no *.transcript.txt
#
# The transcript is WRITTEN under the `.aborted.txt` name and renamed only at
# `capture_complete`, so no signal can leave a receipt from a run that never
# completed. When the checker passes its own path (arg 2, no `.transcript.txt`
# suffix) nothing is renamed.
#
# Usage:
#   ./journey-J3-marco.sh <path-to-rustain> [transcript-path]
#
# Requires bash 4+, python3 with pexpect+pyte (the tests_tui harness), GNU date
# and `git` (the Resolution beat clones a real local bare repository).
set -uo pipefail

BIN="${1:?usage: journey-J3-marco.sh <path-to-rustain> [transcript-path]}"
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
STAMP="$(date -u +%Y-%m-%dT%H-%M-%SZ)"
RECORDED_AT="$(printf '%s' "$STAMP" | sed 's/T\(..\)-\(..\)-\(..\)Z/T\1:\2:\3Z/')"
HERE="$(cd "$(dirname "$0")" && pwd)"
TRANSCRIPT="${2:-$HERE/receipts/journey-J3-${STAMP}.transcript.txt}"
CAPTURED=""
PENDING="$TRANSCRIPT"
[ "$TRANSCRIPT" = "${TRANSCRIPT%.transcript.txt}" ] || PENDING="${TRANSCRIPT%.transcript.txt}.aborted.txt"

SCENE="$HERE/scenes/j3-marco.json"
SKILL="$HERE/skills/safe-deploy/SKILL.md"
RUSTAIN_ROOT="${RUSTAIN_ROOT:-$(cd "$(dirname "$BIN")/../.." && pwd)}"
SCENE_PROVIDER="$RUSTAIN_ROOT/tests_tui/fixtures/scene_provider.py"
RUN="$(mktemp -d)"
WS="$RUN/workspace"
WS_DECLINE="$RUN/workspace-decline"
HOME_DIR="$RUN/home"
SCRATCH="$RUN/scratch"
STUB_LOG="$RUN/requests.jsonl"
STUB_OUT="$RUN/stub.out"
STUB_PID=""
SESSION_STATUS=0

command -v python3 >/dev/null || { echo "FAIL: python3 is required"; exit 1; }
command -v git >/dev/null || { echo "FAIL: git is required (the Resolution beat clones a repo)"; exit 1; }
date -u -d @0 +%s >/dev/null 2>&1 || { echo "FAIL: GNU date (-d) is required"; exit 1; }
python3 -c "import pexpect, pyte" 2>/dev/null || { echo "FAIL: python3 pexpect+pyte is required (tests_tui venv)"; exit 1; }
[ -f "$SCENE_PROVIDER" ] || { echo "FAIL: scene provider missing at $SCENE_PROVIDER (19.7)"; exit 1; }
[ -f "$SCENE" ] || { echo "FAIL: scene missing at $SCENE"; exit 1; }
[ -f "$SKILL" ] || { echo "FAIL: the PRD's SKILL.md fixture is missing at $SKILL"; exit 1; }
mkdir -p "$WS" "$WS_DECLINE" "$HOME_DIR" "$SCRATCH" "$HERE/receipts"
TRANSCRIPT_DIR="$(dirname "$TRANSCRIPT")"
[ ! -d "$TRANSCRIPT" ] || { echo "FAIL: transcript path is a directory: $TRANSCRIPT"; exit 1; }
[ ! -d "$PENDING" ] || { echo "FAIL: pending transcript path is a directory: $PENDING"; exit 1; }
[ -d "$TRANSCRIPT_DIR" ] || { echo "FAIL: transcript directory missing at $TRANSCRIPT_DIR"; exit 1; }
[ -w "$TRANSCRIPT_DIR" ] || { echo "FAIL: transcript directory is not writable at $TRANSCRIPT_DIR"; exit 1; }

# ── The stub, up first: its banner carries the port and the scene hash ──────
python3 "$SCENE_PROVIDER" --scene "$SCENE" --port 0 --log "$STUB_LOG" >"$STUB_OUT" 2>"$RUN/stub.err" &
STUB_PID=$!
for _ in $(seq 1 100); do
  [ "$(wc -l <"$STUB_OUT" 2>/dev/null || echo 0)" -ge 2 ] && break
  if ! kill -0 "$STUB_PID" 2>/dev/null; then break; fi
  sleep 0.1
done
STUB_URL="$(sed -n '1s/^listening //p' "$STUB_OUT")"
SCENE_SHA="$(sed -n '2s/^scene sha256://p' "$STUB_OUT")"
if [ -z "$STUB_URL" ] || [ -z "$SCENE_SHA" ]; then
  echo "FAIL: the scene provider did not announce itself; stderr:"
  cat "$RUN/stub.err" 2>/dev/null
  kill "$STUB_PID" 2>/dev/null
  exit 1
fi
SKILL_SHA="$(sha256sum "$SKILL" | awk '{print $1}')"

# The `tee` ignores INT/TERM/PIPE so it outlives a signal aimed at the whole
# process group (19.7 A9 shape — a dead tee means SIGPIPE, exit 141, no trap).
exec > >(trap '' INT TERM PIPE; tee "$PENDING") 2>&1

say() { printf '\n=== %s\n' "$*"; }

cleanup() {
  trap '' PIPE
  if [ -n "$STUB_PID" ] && kill -0 "$STUB_PID" 2>/dev/null; then
    kill -TERM "$STUB_PID" 2>/dev/null
  fi
  say "cleanup"
  [ -n "$CAPTURED" ] || say "aborted before capture — partial output at $PENDING"
  if [ -n "$CAPTURED" ] && [ -d "$RUN" ]; then
    rm -rf "$RUN"
  fi
  sleep 1
}
trap cleanup EXIT
trap 'exit 130' INT TERM

capture_complete() {
  if [ "$PENDING" != "$TRANSCRIPT" ]; then
    if ! mv -- "$PENDING" "$TRANSCRIPT"; then
      say "capture finalization failed — output remains at $PENDING"
      return 1
    fi
  fi
  CAPTURED=1
  say "captured"
  printf 'transcript: %s\n' "$TRANSCRIPT"
}

# ── Receipt header ───────────────────────────────────────────────────────────
# `tree:` (Story 19.9 A15) names the binary's source tree; `-dirty` is scoped to
# the binary's INPUTS (src/, Cargo.toml, Cargo.lock), because `describe --dirty`
# would flag the not-yet-committed driver files themselves and every first
# recording would say `-dirty` forever.
TREE="$(git -C "$RUSTAIN_ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
if [ -n "$(git -C "$RUSTAIN_ROOT" status --porcelain -- src/ Cargo.toml Cargo.lock 2>/dev/null)" ]; then
  TREE="${TREE}-dirty"
fi
printf 'journey J3 — Marco'"'"'s runbook-as-skill, enforced by the real binary (Story 19.10, gate J3)\n'
printf 'recorded-at: %s\n' "$RECORDED_AT"
printf 'binary:      %s\n' "$BIN"
printf 'version:     %s\n' "$("$BIN" --version 2>&1 | head -1)"
printf 'tree:        rustain@%s\n' "$TREE"
printf 'scene:       tests_tui/journeys/j3/scenes/j3-marco.json sha256:%s\n' "$SCENE_SHA"
printf 'skill:       tests_tui/journeys/j3/skills/safe-deploy/SKILL.md sha256:%s\n' "$SKILL_SHA"
printf 'run dir:     %s\n' "$RUN"

# ── The walk ─────────────────────────────────────────────────────────────────
export RUSTAIN_TUI_BINARY="$BIN"
say "journey_j3_marco.py — the beats"
if python3 "$HERE/journey_j3_marco.py" "$WS" "$STUB_URL" "$HOME_DIR" "$WS_DECLINE" "$SCRATCH"; then
  SCRIPT_STATUS=0
else
  SCRIPT_STATUS=$?
  say "the pexpect walk failed (exit $SCRIPT_STATUS)"
fi

# ── The three receipt-integrity trailers, in this order ─────────────────────
say "--- ledger --- (product-minted milliseconds, one row per stream POST; the title call writes none)"
cat "$WS"/.rustain_data/usage/*.jsonl 2>/dev/null || echo "(no ledger rows)"
say "--- sessions --- (ids, titles, is_default_resume — session list is cwd-bound)"
if ( cd "$WS" && "$BIN" session list --json ); then
  SESSION_STATUS=0
else
  SESSION_STATUS=$?
  say "session list failed (exit $SESSION_STATUS)"
fi
say "--- requests --- (every request the binary made, in order)"
cat "$STUB_LOG"

# ── What this run did not prove (AC6) — a reading aid; no gate greps it ─────
say "--- what this run did not prove ---"
cat <<'LIMITS'
helm diff / kubectl: NOT CAPTURED. `Bash(kubectl:*)` and `Bash(helm:*)` are
  pattern items this build does not expand — `tool_survives_allowlist` compares
  exact tool names — so `Bash` is filtered OUT of submission 2's catalogue
  rather than admitted for those two commands. Owner: row 19.11
  (DF-19-2-BASH-PATTERN-ALLOWLIST). Two corrections to that entry's blast
  radius, both found at this story's preflight: protocol step 1 ("Read
  deploy.yaml") IS reachable and IS captured, and so is step 3's READ of a named
  migration, because `Read` is the one honoured item. But step 3 is "check
  migrations/ for pending changes" — a DIRECTORY SCAN — and there is no Glob,
  Grep or LS tool in this catalogue at all. That is not an allowlist consequence
  and row 19.11 will not fix it. What this receipt proves is a Read of a
  scene-named file.

the STOP-and-warn text is STUB-SERVED. "Breaking migration detected … Are you
  sure?" is Journey 3's emotional climax and it is prose THIS STORY wrote into
  `j3-marco.json`. The product rendered it; no model reasoned it. Nothing about
  the JUDGEMENT is asserted anywhere. What is asserted is that the migration
  file was READ — the driver-written needle round-tripped to the provider.

skill selection was SCRIPTED. The scene names `safe-deploy` because this story
  wrote it there; the model was shown no skill catalogue. `SkillExposurePort::
  render` — tier 1 of FR41 — is composed, bound, startup-validated,
  CLI-overridable and telemetry-instrumented, and has ZERO non-test call sites,
  so the PRD's 2026-08-23 amendment ("the model selects it from the L1
  description") is untrue of this build. DF-19-10-L1-CATALOG-UNWIRED, registered
  as a named decision item in the Epic 19 close shape. What this proves is that
  activation BY NAME works and is trust-gated; it does not prove auto-selection.

same-turn EXECUTION enforcement is immediate. After model-driven activation,
  `run_turn` refreshes the live conversation activation set before scheduling
  the next call. The probe therefore receives the product's skill-policy
  denial before Elevated-risk approval, and `J3-MARCO-MIDTURN` never executes.
  The offered catalogue and `<skill>` prompt still refresh on the next user
  submission; this receipt does not claim mid-request prompt recomposition.

no permission prompt paints, and that is the expected enforcement path. `Read`
  is `ToolRisk::Safe`; the same-turn `Bash` probe is denied by skill policy
  before the risk gate; and `Bash` is absent from submission 2's catalogue.
  The `allowed_tools = []` config and deleted `.env` remove approval and
  credential confounds rather than creating evidence.

the disclosure is SILENT in the default density mode, and this capture had to
  change modes to photograph it. FR42-a's Advisory is routed through
  `apply_warning_notice` -> `notify_or_queue`, and in `DensityMode::Focus` —
  the DEFAULT (`visual.rs:25-29`) — that function ENQUEUES the notice instead
  of rendering it (`handlers/notice.rs:25-35`). The only drain anywhere in the
  tree is `apply_density_transition`'s `if leaving_focus`
  (`handlers/notice.rs:104-142`). So on a default-mode rustain the sentence
  FR42-a exists to say is minted, put in a bounded queue (cap 32, oldest
  dropped) and never shown. Measured, not inferred: the first recording of this
  capture carried `"tools":3` on the wire and nothing on screen. This receipt
  therefore presses `Ctrl+X` `w` — a real operator action — before submission 2,
  and the `Loaded 1 skills` notice you can see draining beside the Advisory is
  the same queue emptying. DF-19-10-ADVISORY-QUEUED-IN-FOCUS remains open;
  whether an FR42-a Advisory should bypass the density queue the way an Error
  does is a separate product decision.

`deploy-read` and `migration-read` are BLOCK HEADERS, not expanded bodies, and
  the reason is a product gap. `Tab`
  (`CycleInvocationInFocusedTurn`, `event_loop.rs:5153-5162`) only cycles tool
  blocks when `view_state.focused_turn` is set, and the only thing that sets it
  is `]]`/`[[`, whose `start_ref` is `focused.or(topmost_on_screen)`
  (`event_loop.rs:4909`). A conversation with exactly ONE assistant turn makes
  that turn its own `topmost_on_screen`, so `]]` searches strictly after it and
  `[[` strictly before it, both find nothing, and focus is never seated. J0's
  own honesty block says "every block is expandable by keyboard as of this
  tree"; that claim has this hole. DF-19-10-SINGLE-TURN-FOCUS-UNREACHABLE.
  Nothing is lost from the evidence: the read-backs are asserted on the wire.

trust is PER-CONVERSATION and MEMORY-ONLY. Launch 2 uses `--new` after a
  relaunch and creates a second persisted session, so it demonstrates only
  that trust is not carried across that boundary — behaviour consistent with
  the prompt's "for this session" wording. This receipt does NOT prove the
  wording defect. The separate code-inspection finding is that a second tab in
  one running app has a different `conversation_id` and re-prompts:
  DF-19-10-TRUST-PROMPT-SAYS-SESSION.

tier 3 is NOT captured. No bundled-resource read happens here, and a
  MODEL-driven activation never calls `SecurityPort::add_active_skill_dir` — the
  user-driven `/skill` route does (`event_loop.rs:7733`). Harmless in this
  capture because the skill lives under the workspace; load-bearing for
  `~/.agents/skills`.

cross-tool compatibility is NOT proved and cannot be. "The skill works in Claude
  Code, Codex, Gemini CLI AND rustain" is a claim about four binaries; this is a
  single-binary capture. What it proves is that the standard's file format, as
  the PRD writes it, loads here unmodified.

`task` is offered and then denied — `tool_survives_allowlist` carves out both
  `activate_skill` and `task`, `permission_chain` carves out `activate_skill`
  only (DF-19-2-TASK-CARVEOUT-DIVERGENCE). Not exercised here.

waits on STUB-SERVED strings, named because A12 requires it: "Breaking migration
  detected" (the climax reply), "Standing by on the preview step." (submission
  2's reply), "The team runbooks are in place." (launch 2's reply),
  "Understood." (the decline leg's reply), and the two `Read` block headers
  `deploy.yaml` / `0042_drop_column.sql`, which are the scene's tool INPUTS
  rendered back. Every other wait in this driver is on a product-minted string:
  `Ready`, `/safe-deploy` (the slash popup, from the registry's own discovery),
  `Trust and enable this skill for this session?`, `Inspect skill:`,
  `Skill 'safe-deploy' activated.`, `cannot be honoured in full`,
  `Discovered skills:` and `not trusted — activation declined`.

no live-LLM variant is recorded by anyone, and none is gated.
LIMITS

# ── Stub teardown, then the verdict is the checker's, not this file's ───────
say "stopping the scene provider"
kill -TERM "$STUB_PID" 2>/dev/null
wait "$STUB_PID" 2>/dev/null
STUB_STATUS=$?
STUB_PID=""
say "scene provider exit status: $STUB_STATUS (0 clean; 1 desync or unknown path)"

EXIT_OR=$(( SCRIPT_STATUS | STUB_STATUS | SESSION_STATUS ))
if [ "$EXIT_OR" -eq 0 ]; then
  capture_complete
  FINALIZE_STATUS=$?
  EXIT_OR=$(( EXIT_OR | FINALIZE_STATUS ))
else
  say "not captured (exit $EXIT_OR) — output stays at $PENDING"
fi
exit "$EXIT_OR"
