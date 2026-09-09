#!/usr/bin/env bash
#
# Epic 19 journey gate J1 — Kai's day in one session of the real binary
# (Story 19.8): tabs, auto-title, @file, plan card + y, /command, fork,
# model switcher, context meter — against the deterministic scene provider
# (Story 19.7), asserted by `check-epic-close-gates.sh`'s `gate J1` section.
#
# # What this driver owns, and what it does not
#
# This shell owns the stub lifecycle, the receipt header (including the
# `scene:` hash line that makes the capture replayable), the pexpect walk
# (`journey_j1_kai.py`) and the three receipt-integrity trailers — the usage
# ledger (product-minted milliseconds), `session list --json` (ids, titles,
# `has_fork_source`) and the stub's request log (every request the binary
# made). The SCREEN is a witness, never the evidence: a gate assertion may
# not rest on a string the stub was told to serve and the binary merely
# rendered (19.7 A4).
#
# # Honest limits, printed in the receipt's final block
#
# * The History sidebar is NOT CAPTURED in this tree: raw Ctrl+H (\x08) is
#   Backspace under a PTY, the palette "toggle sidebar" route no-ops, and the
#   Ctrl+X chord map has no History entry — `DF-19-8-CTRLH-PTY`. The gate
#   asserts the marker anyway and reads FAILED (executed) until that DF lands
#   (Story 19.8 ruling A1).
# * Plan execution is recorded up to its first post-approval POST (exactly 1
#   for this one-task scene), not asserted (A6).
# * The model switcher is an overlay-open/overlay-closed screen-state marker
#   only; no model name is a needle (A7).
# * 19.7's stub serves a FIXED title ("Scene capture"); the quote-strip
#   transformation of ruling A4 is not exercised (owner ruling 2026-08-26).
# * No live-LLM variant is recorded by anyone, and none is gated.
#
# # Aborted runs leave no *.transcript.txt
#
# Same hook as the 19.7 drivers: the transcript is WRITTEN under the
# `.aborted.txt` name and renamed only at `capture_complete`, so no signal can
# leave a receipt from a run that never completed. When the checker passes its
# own path (arg 2, no `.transcript.txt` suffix) nothing is renamed.
#
# Usage:
#   ./journey-J1-kai.sh <path-to-rustain> [transcript-path]
#
# Requires bash 4+, python3 with pexpect+pyte (the tests_tui harness), GNU
# date. The stub is located relative to the binary (19.7 A7 direction).
set -uo pipefail

BIN="${1:?usage: journey-J1-kai.sh <path-to-rustain> [transcript-path]}"
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
STAMP="$(date -u +%Y-%m-%dT%H-%M-%SZ)"
RECORDED_AT="$(printf '%s' "$STAMP" | sed 's/T\(..\)-\(..\)-\(..\)Z/T\1:\2:\3Z/')"
HERE="$(cd "$(dirname "$0")" && pwd)"
TRANSCRIPT="${2:-$HERE/receipts/journey-J1-${STAMP}.transcript.txt}"
CAPTURED=""
PENDING="$TRANSCRIPT"
[ "$TRANSCRIPT" = "${TRANSCRIPT%.transcript.txt}" ] || PENDING="${TRANSCRIPT%.transcript.txt}.aborted.txt"

SCENE="$HERE/scenes/j1-kai.json"
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
  # A captured run's evidence is the receipt itself; only a failed run is
  # worth debugging from the run dir, so only a failed run keeps it.
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
printf 'journey J1 — Kai daily driver, one session of the real binary (Story 19.8, gate J1)\n'
printf 'recorded-at: %s\n' "$RECORDED_AT"
printf 'binary:      %s\n' "$BIN"
printf 'version:     %s\n' "$("$BIN" --version 2>&1 | head -1)"
printf 'scene:       tests_tui/journeys/j1/scenes/j1-kai.json sha256:%s\n' "$SCENE_SHA"
printf 'run dir:     %s\n' "$RUN"

# ── The walk ─────────────────────────────────────────────────────────────────
export RUSTAIN_TUI_BINARY="$BIN"
say "journey_j1_kai.py — the beats"
if python3 "$HERE/journey_j1_kai.py" "$WS" "$STUB_URL" "$HOME_DIR"; then
  SCRIPT_STATUS=0
else
  SCRIPT_STATUS=$?
  say "the walk FAILED (exit $SCRIPT_STATUS) — panes above show where"
fi

# ── The three receipt-integrity trailers, in this order ─────────────────────
say "--- ledger --- (product-minted milliseconds, one row per stream POST; the title call writes none)"
cat "$WS"/.rustain_data/usage/*.jsonl 2>/dev/null || echo "(no ledger rows)"
say "--- sessions --- (ids, titles, has_fork_source — session list is cwd-bound)"
( cd "$WS" && "$BIN" session list --json )
say "--- requests --- (every request the binary made, in order)"
cat "$STUB_LOG"

# ── What this run did not prove (AC4) — a reading aid; no gate greps it ─────
cat <<'LIMITS'

--- what this run did not prove ---
Ctrl+H: palette route attempted ("toggle sidebar"); raw \x08 is Backspace under
  a PTY and the palette route no-ops in this tree — the History sidebar did
  NOT open: NOT CAPTURED (DF-19-8-CTRLH-PTY)
plan execution: 1 post-approval POST recorded, not asserted (A6)
model switcher: overlay only, no model asserted (A7)
title: served fixed "Scene capture" by the 19.7 stub — the quote-strip
  transformation (A4) is not exercised (owner ruling 2026-08-26: adapt)
live-LLM variant: not recorded
LIMITS

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
