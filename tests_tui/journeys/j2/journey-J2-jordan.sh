#!/usr/bin/env bash
#
# Epic 19 journey gate J2 — Jordan the skeptic, audited by the real binary
# (Story 19.9): a `.claude/agents/*.md` file dropped in and activated with
# `@Agents/`, a markdown findings reply, a `Bash` and a `Write` that each raise a
# real permission prompt, `y`, the INLINE LINE DIFF in the expanded block (Story
# 19.1), a sonnet↔haiku model switch and a provider switch onto a SECOND WIRE —
# the OpenAI-compat `/v1/chat/completions` arm the scene provider gained in this
# story (ruling A11) — asserted by `check-epic-close-gates.sh`'s `gate J2`.
#
# # The front door is the CONFIG provider path, on purpose
#
# `gate J0` captures the env path (`ANTHROPIC_BASE_URL` + `ANTHROPIC_API_KEY`,
# no `[provider.*]` at all) because that *is* Sam's claim. A provider switch has
# nothing to switch between on that path, so J2 writes `.rustain/config.toml`
# with two stubbed providers before the TUI spawns: `scene` (kind `anthropic`,
# `POST {base_url}/v1/messages`) and `openrouter` (kind `openrouter`, i.e. the
# OpenAI-compat adapter, `POST {base_url}/chat/completions`). Both point at the
# same stub process; the env block carries `SCENE_API_KEY` and
# `OPENROUTER_API_KEY` and NOT `ANTHROPIC_*`, because two front doors in one
# capture prove neither.
#
# # What this driver owns, and what it does not
#
# This shell owns the stub lifecycle, the receipt header (including the `tree:`
# line that names the binary's source tree and the `scene:` hash line that makes
# the capture replayable), the pexpect walk (`journey_j2_jordan.py`) and the three
# receipt-integrity trailers — the usage ledger (product-minted milliseconds),
# `session list --json` and the stub's request log (every request the binary
# made). The SCREEN is a witness, never the evidence, with ONE deliberate
# exception (Story 19.9 A2): the expanded diff's `- <old line>` row is a string
# the DRIVER wrote to disk, the BINARY snapshotted, read back and rendered. The
# stub never served it — `grep -c J2-JORDAN-OLD-LINE scenes/j2-jordan.json` is 0,
# and the gate checks that precondition first.
#
# # Honest limits, printed in the receipt's final block
#
# * The diff is shown AFTER execution, inside the expanded block. The approval
#   prompt renders the tool INPUT only, so the PRD's "reads the diff inline …
#   presses y" sentence order is aspiration; the shipped order is prompt → `y`
#   → executed block → expand → diff.
# * The PRD's `curl https://advisory-db…` is replaced by a local `printf`: a
#   deterministic capture makes no network call. The prompt and the stdout
#   round-trip are what J2 actually claims.
# * OpenRouter is captured against the stub's OpenAI-wire arm; the live
#   openrouter.ai wire is not exercised.
# * Markdown styling is invisible under `NO_COLOR` + pyte — only the TEXT
#   survives, and it is asserted as a positive control only.
# * Agent activation is a persona/tool-filter/model swap, not a subagent
#   dispatch: the ` Agents ` panel is never asserted for it.
# * No live-LLM variant is recorded by anyone, and none is gated.
#
# # Aborted runs leave no *.transcript.txt
#
# The transcript is WRITTEN under the `.aborted.txt` name and renamed only at
# `capture_complete`, so no signal can leave a receipt from a run that never
# completed. When the checker passes its own path (arg 2, no `.transcript.txt`
# suffix) nothing is renamed.
#
# Usage:
#   ./journey-J2-jordan.sh <path-to-rustain> [transcript-path]
#
# Requires bash 4+, python3 with pexpect+pyte (the tests_tui harness), GNU date.
# The stub is located relative to the binary (19.7 A7 direction).
set -uo pipefail

BIN="${1:?usage: journey-J2-jordan.sh <path-to-rustain> [transcript-path]}"
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
STAMP="$(date -u +%Y-%m-%dT%H-%M-%SZ)"
RECORDED_AT="$(printf '%s' "$STAMP" | sed 's/T\(..\)-\(..\)-\(..\)Z/T\1:\2:\3Z/')"
HERE="$(cd "$(dirname "$0")" && pwd)"
TRANSCRIPT="${2:-$HERE/receipts/journey-J2-${STAMP}.transcript.txt}"
CAPTURED=""
PENDING="$TRANSCRIPT"
[ "$TRANSCRIPT" = "${TRANSCRIPT%.transcript.txt}" ] || PENDING="${TRANSCRIPT%.transcript.txt}.aborted.txt"

SCENE="$HERE/scenes/j2-jordan.json"
RUSTAIN_ROOT="${RUSTAIN_ROOT:-$(cd "$(dirname "$BIN")/../.." && pwd)}"
SCENE_PROVIDER="$RUSTAIN_ROOT/tests_tui/fixtures/scene_provider.py"
RUN="$(mktemp -d)"
WS="$RUN/workspace"
HOME_DIR="$RUN/home"
STUB_LOG="$RUN/requests.jsonl"
STUB_OUT="$RUN/stub.out"
STUB_PID=""

command -v python3 >/dev/null || { echo "FAIL: python3 is required"; exit 1; }
date -u -d @0 +%s >/dev/null 2>&1 || { echo "FAIL: GNU date (-d) is required"; exit 1; }
python3 -c "import pexpect, pyte" 2>/dev/null || { echo "FAIL: python3 pexpect+pyte is required (tests_tui venv)"; exit 1; }
[ -f "$SCENE_PROVIDER" ] || { echo "FAIL: scene provider missing at $SCENE_PROVIDER (19.7)"; exit 1; }
[ -f "$SCENE" ] || { echo "FAIL: scene missing at $SCENE"; exit 1; }
mkdir -p "$WS" "$HOME_DIR" "$HERE/receipts"

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
  say "captured"
  printf 'transcript: %s\n' "$TRANSCRIPT"
  CAPTURED=1
  [ "$PENDING" = "$TRANSCRIPT" ] || mv "$PENDING" "$TRANSCRIPT"
}

# ── Receipt header ───────────────────────────────────────────────────────────
# `tree:` (Story 19.9 A15) is the line the J1 header lacks: a `version:` of
# `rustain 0.1.2` cannot tell a pre-19.1 binary from a post-19.1 one, and this
# capture's whole claim is that the binary computed a diff. `-dirty` is scoped to
# the binary's INPUTS (src/, Cargo.toml, Cargo.lock) — `describe --dirty` would
# flag the not-yet-committed driver files themselves, so every first recording
# would say `-dirty` forever.
TREE="$(git -C "$RUSTAIN_ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
if [ -n "$(git -C "$RUSTAIN_ROOT" status --porcelain -- src/ Cargo.toml Cargo.lock 2>/dev/null)" ]; then
  TREE="${TREE}-dirty"
fi
printf 'journey J2 — Jordan the skeptic, audited by the real binary (Story 19.9, gate J2)\n'
printf 'recorded-at: %s\n' "$RECORDED_AT"
printf 'binary:      %s\n' "$BIN"
printf 'version:     %s\n' "$("$BIN" --version 2>&1 | head -1)"
printf 'tree:        rustain@%s\n' "$TREE"
printf 'scene:       tests_tui/journeys/j2/scenes/j2-jordan.json sha256:%s\n' "$SCENE_SHA"
printf 'run dir:     %s\n' "$RUN"

# ── The walk ─────────────────────────────────────────────────────────────────
export RUSTAIN_TUI_BINARY="$BIN"
say "journey_j2_jordan.py — the beats"
if python3 "$HERE/journey_j2_jordan.py" "$WS" "$STUB_URL" "$HOME_DIR"; then
  SCRIPT_STATUS=0
else
  SCRIPT_STATUS=$?
  say "the pexpect walk failed (exit $SCRIPT_STATUS)"
fi

# ── The three receipt-integrity trailers, in this order ─────────────────────
say "--- ledger --- (product-minted milliseconds, one row per stream POST; the title call writes none)"
cat "$WS"/.rustain_data/usage/*.jsonl 2>/dev/null || echo "(no ledger rows)"
say "--- sessions --- (ids, titles, is_default_resume — session list is cwd-bound)"
( cd "$WS" && "$BIN" session list --json )
say "--- requests --- (every request the binary made, in order)"
cat "$STUB_LOG"

# ── What this run did not prove (AC5) — a reading aid; no gate greps it ─────
say "--- what this run did not prove ---"
cat <<'LIMITS'
diff timing: shown in the expanded block AFTER execution; the approval prompt
  renders the tool input only
front door: CONFIG provider path (two stubbed providers, scene + openrouter) —
  gate J0 is the env-path capture; the harness-copied .env was deleted before
  the first keystroke and both allow-lists are EMPTY
OpenRouter: captured against the stub's OpenAI-wire arm (scene_provider.py +
  tests_tui/test_story_19_9_openai_wire_arm.py) on the config provider path —
  the live openrouter.ai wire is NOT exercised
Bash: the PRD's curl is replaced by a local printf — offline capture; the
  permission prompt and the stdout round-trip are what J2 claims
markdown: styling is invisible under NO_COLOR/pyte — the fence content and the
  heading text are asserted as a positive control only, paired with the tools
  count that proves the agent filter reached the wire
model switch: proved from the request log's per-row `model`/`path` fields; the
  on-screen flashes are switch-specific witnesses only (they accumulate, so a
  bare "Switched to" cannot tell them apart)
agents: activation is a persona / tool-filter / model swap, not a subagent
  dispatch — the ` Agents ` panel is never asserted for it
tool-block focus: every block is expandable by keyboard as of this tree
  (Story 19.9 A3); the Write opened here is the 3rd tool call and the Read the
  1st, each reached by ]] to its turn and Tab to its invocation
  accepted limit: keyboard focus follows a block's FIRST line (start-anchored
  visibility), so a block expanded taller than the 30-row pane releases focus
  once its ┌─ header scrolls above the viewport top, and Enter cannot collapse
  it until its start scrolls back into view
fixture content: the scripted fix (`unwrap_or(0)`) was chosen for its diff
  shape and A16's <=60-character line limit; it silently defaults invalid
  input to 0 and is NOT a recommended remediation for the unchecked unwrap
  the audit reports
live-LLM variant: not recorded
LIMITS
printf 'input line: '
sed -n '/--- pane: fix-expanded ---/,/--- end pane ---/p' "$PENDING" \
  | grep -m1 -F '{"content"' || printf '(no input line captured)\n'
printf '  ^ DF-19-1-INPUT-LINE-DUMP evidence, not fixed: the compact tool input is\n'
printf '    dumped verbatim and truncated at the 130-column pane\n'

# ── Stub teardown, then the verdict is the checker's, not this file's ───────
say "stopping the scene provider"
kill -TERM "$STUB_PID" 2>/dev/null
wait "$STUB_PID" 2>/dev/null
STUB_STATUS=$?
STUB_PID=""
say "scene provider exit status: $STUB_STATUS (0 clean; 1 desync or unknown path)"

EXIT_OR=$(( SCRIPT_STATUS | STUB_STATUS ))
if [ "$EXIT_OR" -eq 0 ]; then
  capture_complete
else
  say "not captured (exit $EXIT_OR) — output stays at $PENDING"
fi
exit "$EXIT_OR"
