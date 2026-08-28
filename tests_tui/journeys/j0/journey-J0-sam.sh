#!/usr/bin/env bash
#
# Epic 19 journey gate J0 — Sam's first run of the real binary (Story 19.9):
# nothing but an env key, streaming, real Read tool blocks, a Write that raises
# a real permission prompt, `y`, the INLINE LINE DIFF in the expanded block
# (Story 19.1), Ctrl+Q (Story 19.3), and a second launch that restores the
# session from .meta.json — against the deterministic scene provider (Story
# 19.7), asserted by `check-epic-close-gates.sh`'s `gate J0` section.
#
# # What this driver owns, and what it does not
#
# This shell owns the stub lifecycle, the receipt header (including the `tree:`
# line that names the binary's source tree and the `scene:` hash line that makes
# the capture replayable), the pexpect walk (`journey_j0_sam.py`) and the three
# receipt-integrity trailers — the usage ledger (product-minted milliseconds),
# `session list --json` (ids, titles, resume flag) and the stub's request log
# (every request the binary made). The SCREEN is a witness, never the evidence,
# with ONE deliberate exception (Story 19.9 A2): the expanded diff's `- <old
# line>` row is a string the DRIVER wrote to disk, the BINARY snapshotted, read
# back and rendered. The stub never served it — `grep -c J0-SAM-OLD-LINE
# scenes/j0-sam.json` is 0, and the gate checks that precondition first.
#
# # Honest limits, printed in the receipt's final block
#
# * The diff is shown AFTER execution, inside the expanded block. The approval
#   prompt renders the tool INPUT only, so the PRD's "shows the diff inline. A
#   permission prompt appears" sentence order is aspiration; the shipped order
#   is prompt → `y` → executed block → expand → diff.
# * PRD J0's second tool call is a `Glob`. There is no `Glob` tool in this
#   product, so the beat is two real `Read`s over two driver-written files.
# * The `<100ms` first frame is not measured: a pexpect boot timing is a host
#   measurement, not a product claim.
# * vim `j`/`k`/`i` is recorded, not asserted (`tests/scroll.rs` owns it).
# * The expanded block's input line is recorded verbatim as capture-side
#   evidence for `DF-19-1-INPUT-LINE-DUMP`; the widget is not touched here.
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
#   ./journey-J0-sam.sh <path-to-rustain> [transcript-path]
#
# Requires bash 4+, python3 with pexpect+pyte (the tests_tui harness), GNU date.
# The stub is located relative to the binary (19.7 A7 direction).
set -uo pipefail

BIN="${1:?usage: journey-J0-sam.sh <path-to-rustain> [transcript-path]}"
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
STAMP="$(date -u +%Y-%m-%dT%H-%M-%SZ)"
HERE="$(cd "$(dirname "$0")" && pwd)"
TRANSCRIPT="${2:-$HERE/receipts/journey-J0-${STAMP}.transcript.txt}"
CAPTURED=""
PENDING="$TRANSCRIPT"
[ "$TRANSCRIPT" = "${TRANSCRIPT%.transcript.txt}" ] || PENDING="${TRANSCRIPT%.transcript.txt}.aborted.txt"

SCENE="$HERE/scenes/j0-sam.json"
RUSTAIN_ROOT="$(cd "$(dirname "$BIN")/../.." && pwd)"
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
printf 'journey J0 — Sam'"'"'s first run of the real binary (Story 19.9, gate J0)\n'
printf 'recorded-at: %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
printf 'binary:      %s\n' "$BIN"
printf 'version:     %s\n' "$("$BIN" --version 2>&1 | head -1)"
printf 'tree:        rustain@%s\n' "$TREE"
printf 'scene:       tests_tui/journeys/j0/scenes/j0-sam.json sha256:%s\n' "$SCENE_SHA"
printf 'run dir:     %s\n' "$RUN"

# ── The walk ─────────────────────────────────────────────────────────────────
export RUSTAIN_TUI_BINARY="$BIN"
say "journey_j0_sam.py — the beats"
if python3 "$HERE/journey_j0_sam.py" "$WS" "$STUB_URL" "$HOME_DIR"; then
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
first-run config: the harness wrote .rustain/config.toml and .claude/settings.json
  with EMPTY allow-lists; the copied .env was deleted before the first keystroke
Glob: there is no Glob tool in this product (15 tool definitions on the wire,
  none of them Glob) — PRD J0's second tool call is a second real Read over a
  second driver-written file; the needle that came back is J0-SAM-LIB-NEEDLE
<100ms first frame: not measured (host timing)
vim j/k/i: recorded, not asserted
tool-block focus: every block is expandable by keyboard as of this tree
  (Story 19.9 A3); the Write opened here is the 4th tool call, reached by
  ]] to its turn and Tab to its invocation
live-LLM variant: not recorded
LIMITS
printf 'input line: '
sed -n '/--- pane: write-expanded ---/,/--- end pane ---/p' "$PENDING" \
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
