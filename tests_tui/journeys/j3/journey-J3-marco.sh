#!/usr/bin/env bash
#
# Epic 19 journey gate J3 — Marco's runbook-as-skill, enforced by the real binary
# (Story 19.11): the PRD's own `.agents/skills/safe-deploy/SKILL.md`, byte for
# byte, discovered at boot, activated by a model-driven `activate_skill`,
# TRUST-GATED with `[y]`/`[n]`/`[i]`, read to the model as a `<skill>` block,
# then command-gated: declared helm/kubectl commands execute through real
# approval prompts while an undeclared chain is denied. Asserted by
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

SCENE="$HERE/scenes/j3-marco-v2.json"
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
printf 'journey J3 — Marco'"'"'s runbook-as-skill, pattern-gated by the real binary (Story 19.11, gate J3)\n'
printf 'recorded-at: %s\n' "$RECORDED_AT"
printf 'binary:      %s\n' "$BIN"
printf 'version:     %s\n' "$("$BIN" --version 2>&1 | head -1)"
printf 'tree:        rustain@%s\n' "$TREE"
printf 'scene:       tests_tui/journeys/j3/scenes/j3-marco-v2.json sha256:%s\n' "$SCENE_SHA"
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
`helm` and `kubectl` are HERMETIC SHIMS on PATH. Each executable is created by
  the driver and prints its driver-owned needle as its first output line so the
  provider's 120-byte `last_user` clip cannot hide it. This proves rustain
  admitted and executed each pattern-matching Bash command; it proves nothing
  about Helm or Kubernetes. The shim remains executable because the current
  Landlock policies grant read/execute beneath `/`; a future policy that
  narrows that unconditional read grant would invalidate this apparatus.

the chained denial is BLOCKLIST-CLEAN ON PURPOSE. Its second segment is
  `printf`, not `rm -rf`, so the Bash blocklist cannot produce the verdict this
  gate attributes to the quote-aware allowlist segmenter. Exactly two
  command-policy denial rows — the same-turn probe and this chain — are the
  chain's attributable dispatch control. `J3-MARCO-CHAIN-SECOND-RAN` stays
  absent only after that count is proved.

Protocol step 3 still does NOT perform a directory scan. There is no Glob,
  Grep or LS tool in safe-deploy's offered catalogue; the scene names one
  migration file and the binary reads that file. The two read needles prove
  those reads, not full protocol execution.

the STOP-and-warn text is STUB-SERVED. "Breaking migration detected … Are you
  sure?" is Journey 3's emotional climax and prose this scene wrote. The
  product rendered it; no model reasoned it. The gate asserts the migration
  read, not the judgement.

skill selection is SCRIPTED. The scene names `safe-deploy`; the model is shown
  no skill catalogue. `SkillExposurePort::render` still has zero non-test call
  sites (DF-19-10-L1-CATALOG-UNWIRED). This run proves activation by name and
  its trust gate, not automatic selection from tier-1 descriptions.

same-turn EXECUTION enforcement is immediate. After model-driven activation,
  the original `printf` Bash probe receives the product's command-policy denial
  before Elevated-risk approval, and `J3-MARCO-MIDTURN` never executes. The
  offered catalogue and `<skill>` prompt still refresh only on the next user
  submission; this receipt claims no mid-request prompt recomposition.

permission prompts DO paint for admitted Bash. `helm diff` and `kubectl get`
  each reach a real `[y] Allow` card, and the driver answers `y` twice because
  `ApprovalOutcome::Once` does not authorize the next call. The harness keeps
  `allowed_tools = []`; no Bash request is silently pre-approved.

safe-deploy emits NO FR42-a disclosure now that both patterns are enforceable.
  Launch 1 still switches from Focus to Monitor before submission 2, making
  that absence observable rather than an artefact of Focus queueing. The live
  positive control is the cloned `canary-watch` runbook with `Read Glob`; its
  raw intersection with safe-deploy retains `Read`, avoiding the fatal disjoint
  branch, and its unavailable `Glob` produces exactly one Advisory after its
  own trust prompt. Launch 2 also switches to Monitor because
  DF-19-10-ADVISORY-QUEUED-IN-FOCUS remains open.

the old blocking wait on product text `cannot be honoured in` was REMOVED when
  that safe-deploy disclosure became correctly absent. Its product-minted
  replacement is the real Bash approval wait `[y] Allow`; the team-control
  disclosure remains a separate positive FR42-a witness. This is the worked
  example that fires DF-19-9-DRIVER-GATES-ON-SERVED-PROSE; the DF stays open
  with the Epic 19 close.

redirect targets are NOT workspace-checked for Bash. For example,
  `kubectl config view --raw > /tmp/x` still reaches a shell redirect because
  `extract_file_path` handles file tools, not Bash command text. The pattern
  matcher restricts command prefixes; it is not a filesystem security boundary.

invocation-form bypasses remain: `/bin/kubectl`, `bash -c '…'`, flag reordering
  and variable indirection can evade argument-shaped prefix intent. The
  matcher is deliberately a conservative command allowlist, not a shell or
  program policy engine.

`deploy-read` and `migration-read` are BLOCK HEADERS, not expanded bodies,
  because a one-assistant-turn conversation cannot seat `focused_turn` through
  `]]`/`[[`; therefore Tab cannot cycle its tool blocks
  (DF-19-10-SINGLE-TURN-FOCUS-UNREACHABLE). The wire read-backs retain the
  load-bearing evidence.

trust is PER-CONVERSATION and MEMORY-ONLY. Launch 2 creates a new conversation
  and asks for safe-deploy again, behaviour consistent with the current prompt.
  The separate wording/persistence question remains
  DF-19-10-TRUST-PROMPT-SAYS-SESSION.

tier 3 is NOT captured. Model-driven activation does not call
  `SecurityPort::add_active_skill_dir`; this fixture is harmless because it
  lives under the workspace, but the gap remains load-bearing for global
  skills.

agent-side patterns and delegated-agent restrictions are NOT captured by this
  skill-only gate. Story 19.28 adds their command-level permission-chain
  backstop; Rust integration tests own that axis. Delegated children still
  carry no active skills.

cross-tool compatibility is NOT proved. This single-binary capture proves the
  standard file loads unmodified in rustain, not that Claude Code, Codex and
  Gemini CLI execute it.

`task` is offered and then denied — the offer filter carves out `task`, while
  the permission chain carves out only `activate_skill`
  (DF-19-2-TASK-CARVEOUT-DIVERGENCE). It is intentionally not exercised here.

waits on STUB-SERVED strings are: "Breaking migration detected",
  "Standing by on the preview step.", "The team runbooks are in place.",
  "Team control complete.", "Understood.", and the two Read block headers whose
  paths are scene tool inputs. Product-minted waits include `Ready`,
  `/safe-deploy`, both trust prompts, `Inspect skill:`, both Bash `[y] Allow`
  cards, the `Glob` Advisory, `Discovered skills:`, and activation/decline
  results.

no live-LLM variant is recorded or gated.
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
