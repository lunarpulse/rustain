#!/usr/bin/env bash
#
# Journey gate J8 — Ravi's custom-profile authoring and sharing spine, walked by
# the real binary (Story 19.12): the interactive wizard under a PTY, a
# hand-authored partial TOML whose `extends` actually binds, `export`, the new
# `profile install <local path>`, both write-path refusals that landed with it,
# and one real agent turn on the INSTALLED profile. Asserted by
# `check-epic-close-gates.sh`'s `gate J8`.
#
# # Why the capture ends with a turn
#
# The committed-receipt sweep globs `journeys/*/receipts/*.transcript.txt`
# unconditionally, and this file's stem matches `verify_receipt`'s
# `journey-J[0-9]*-*` arm. That arm demands a product-minted ledger millisecond
# and a `scene:` header that hashes. `UsageLedgerPort::append` has three
# production call sites and none of them is in `adapters/cli/`, so NO CLI verb
# can mint one — a CLI-only capture would fail the sweep where `GATE=""`, with a
# global failure and no verdict line explaining it. The turn is therefore forced
# by the checker, and it is the right proof anyway: it is the only beat that
# shows the shared artifact being USED rather than merely moved.
#
# # For J8, a "pane" is a captured CLI output block
#
# `pane_slice` is the checker's one extractor and the four `pane_*` helpers all
# consume it. A CLI capture has no pane, so the driver emits the SAME FENCE BYTES
# around raw child output: zero checker-helper edits, no second extractor, no
# rename (ruling A11). The mechanism is right and the label is a lie of
# convenience — written down here rather than shipped silently.
#
# # What this driver owns, and what it does not
#
# This shell owns the stub lifecycle, the receipt header (the `tree:` line that
# names the binary's source tree, the `scene:` hash that makes the turn
# replayable and the `profile:` hash that names the exact bytes that were
# shared), the walk (`journey_j8_ravi.py`) and the four receipt-integrity
# trailers — the usage ledger, session list, installed profile tree and the
# stub's request log.
#
# ⚑ `recorded-at:` and the filename stamp come from ONE `$STAMP`. J3 calls `date`
# twice, which is the shape `DF-19-5-RECEIPT-NAME-UNVERIFIED` describes (a
# committed J3 receipt is 20h42m adrift from its own filename), and J8 does not
# repeat it in its own new file. ⛔ Scoped to THIS driver: retrofitting the
# others is 19.13's, and re-dating someone else's committed receipt is the
# forgery the sweep exists to expose.
#
# # Honest limits, printed in the receipt's final block
#
# See the `LIMITS` heredoc below. Every line of it is a reading aid and NO gate
# greps any of it — which matters more here than in any previous journey,
# because three of Journey 8's own PRD Capabilities do not exist at all. Those
# three are named in `gate J8`'s verdict line as well, under the rule amendment
# this story recorded in `demos/README.md`.
#
# # Aborted runs leave no *.transcript.txt
#
# The transcript is WRITTEN under the `.aborted.txt` name and renamed only at
# `capture_complete`, so no signal can leave a receipt from a run that never
# completed. When the checker passes its own path (arg 2, no `.transcript.txt`
# suffix) nothing is renamed.
#
# Usage:
#   ./journey-J8-ravi.sh <path-to-rustain> [transcript-path]
#
# Requires bash 4+, python3 with pexpect (the tests_tui harness) and GNU date.
set -uo pipefail

BIN="${1:?usage: journey-J8-ravi.sh <path-to-rustain> [transcript-path]}"
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
# ⚑ ONE stamp for the filename AND the header (see the note above).
STAMP="$(date -u +%Y-%m-%dT%H-%M-%SZ)"
RECORDED_AT="$(printf '%s' "$STAMP" | sed 's/T\(..\)-\(..\)-\(..\)Z/T\1:\2:\3Z/')"
HERE="$(cd "$(dirname "$0")" && pwd)"
TRANSCRIPT="${2:-$HERE/receipts/journey-J8-${STAMP}.transcript.txt}"
CAPTURED=""
PENDING="$TRANSCRIPT"
[ "$TRANSCRIPT" = "${TRANSCRIPT%.transcript.txt}" ] || PENDING="${TRANSCRIPT%.transcript.txt}.aborted.txt"

SCENE="$HERE/scenes/j8-ravi.json"
PROFILE_FIXTURE="$HERE/profiles/devops.toml"
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
python3 -c "import pexpect" 2>/dev/null || { echo "FAIL: python3 pexpect is required (tests_tui venv)"; exit 1; }
python3 -c "import pyte" 2>/dev/null || { echo "FAIL: python3 pyte is required (the turn drives the TUI)"; exit 1; }
[ -f "$SCENE_PROVIDER" ] || { echo "FAIL: scene provider missing at $SCENE_PROVIDER (19.7)"; exit 1; }
[ -f "$SCENE" ] || { echo "FAIL: scene missing at $SCENE"; exit 1; }
[ -f "$PROFILE_FIXTURE" ] || { echo "FAIL: the shared profile fixture is missing at $PROFILE_FIXTURE"; exit 1; }
mkdir -p "$WS" "$HOME_DIR" "$HERE/receipts"
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
PROFILE_SHA="$(sha256sum "$PROFILE_FIXTURE" | awk '{print $1}')"

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
# `tree:` names the binary's source tree; `-dirty` is scoped to the binary's
# INPUTS (src/, Cargo.toml, Cargo.lock), because `describe --dirty` would flag
# the not-yet-committed driver files themselves and every first recording would
# say `-dirty` forever.
#
# ⛔ `scene:` and `profile:` are written relative to `$RUSTAIN_ROOT`, because
# `verify_receipt`'s third resolve candidate is `$RUSTAIN_ROOT/$scene_path`. A
# `$HERE`-relative path would miss all three candidates and the sweep would fail
# on a scene file that is right there.
TREE="$(git -C "$RUSTAIN_ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
if [ -n "$(git -C "$RUSTAIN_ROOT" status --porcelain -- src/ Cargo.toml Cargo.lock 2>/dev/null)" ]; then
  TREE="${TREE}-dirty"
fi
printf 'journey J8 — Ravi'"'"'s custom-profile authoring and sharing spine, walked by the real binary (Story 19.12, gate J8)\n'
printf 'recorded-at: %s\n' "$RECORDED_AT"
printf 'binary:      %s\n' "$BIN"
printf 'version:     %s\n' "$("$BIN" --version 2>&1 | head -1)"
printf 'tree:        rustain@%s\n' "$TREE"
printf 'scene:       tests_tui/journeys/j8/scenes/j8-ravi.json sha256:%s\n' "$SCENE_SHA"
printf 'profile:     tests_tui/journeys/j8/profiles/devops.toml sha256:%s\n' "$PROFILE_SHA"
printf 'run dir:     %s\n' "$RUN"

# ── The walk ─────────────────────────────────────────────────────────────────
export RUSTAIN_TUI_BINARY="$BIN"
say "journey_j8_ravi.py — the ten beats"
if python3 "$HERE/journey_j8_ravi.py" "$WS" "$STUB_URL" "$HOME_DIR" "$RUN"; then
  SCRIPT_STATUS=0
else
  SCRIPT_STATUS=$?
  say "the walk failed with status $SCRIPT_STATUS"
fi

# ── The four receipt-integrity trailers, in this order ──────────────────────
say "--- ledger --- (product-minted milliseconds; one row per stream POST — the CLI beats mint none)"
cat "$WS"/.rustain_data/usage/*.jsonl 2>/dev/null || echo "(no ledger rows)"
say "--- sessions --- (ids, titles, is_default_resume — session list is cwd-bound)"
if ( cd "$WS" && "$BIN" session list --json ); then
  SESSION_STATUS=0
else
  SESSION_STATUS=$?
  say "session list failed (exit $SESSION_STATUS)"
fi
# The filesystem is the evidence for four of the six markers, so it is dumped
# inside a PANE FENCE (ruling A11) rather than as a loose trailer — that gives
# `gate J8` a `pane_grep`/`pane_ungrep` slice instead of a whole-file grep, and
# an absence claim about "under the run dir" stays scoped to the run dir.
#
# ⚑ Rooted at `$RUN`, not at the profiles directory, ON PURPOSE: a removed A20
# guard writes `ESCAPED.toml` ONE LEVEL ABOVE the config dir (measured), so a
# listing scoped to `profiles/` would report the escape as an absence. The
# positive control for both absence claims is that the same listing carries the
# `.toml` files the beats really wrote.
say "--- disk artifacts --- (every .toml and .md under the run dir, after the walk)"
printf -- '--- pane: disk-artifacts ---\n'
find "$RUN" -type f \( -name '*.toml' -o -name '*.md' \) 2>/dev/null \
  | sed "s#^$RUN#<run>#" | sort || echo "(nothing on disk)"
printf -- '--- end pane ---\n'
say "--- requests --- (every request the binary made, in order)"
cat "$STUB_LOG"

# ── What this run did not prove (AC5) — a reading aid; no gate greps it ─────
say "--- what this run did not prove ---"
cat <<'LIMITS'
THE DEVOPS PERSONA DOES NOT EXIST. Journey 8's Opening Scene has Ravi "write a
custom persona (devops.md)", and there is no mechanism for it anywhere in this
build. `build_persona` matches three hardcoded names, and `AdapterRef` carries
`#[serde(default, rename = "config")] pub _config` — so a profile MAY write
`[persona] config = { file = "devops.md" }`, it PARSES, it is carried through
composition, and `build_persona`'s `_config` is never read. A config a user can
write, that validates, and that does nothing: a footgun, not an absence.
`PersonaAdapter::system_prompt` returns the WORKSPACE project context
(CLAUDE.md / .cursorrules), never a profile-scoped file. So this run proves
PROFILE COMPOSITION, not SPECIALISATION — and specialisation is the point of
Ravi's journey. Filed as DF-19-12-PROFILE-PERSONA-MD, an id `epics.md`, the
Epic 19 replan and the sprint tracker were all already citing while it did not
exist.

CRON IS NOT DELIVERABLE THROUGH A PROFILE, and the honest mechanism is narrower
than "silent no-op". In a DEFAULT build a profile naming `[scheduler] cron`
REFUSES TO LOAD: `profile show` and `profile validate` both print "references
adapter 'cron' (port 'Scheduler') which requires cargo feature 'cron'.
Recompile with: cargo install rustain --features cron" and exit 2, and the
wizard warns at selection time. That is why this capture's fixture says
`adapter = "none"` — a cron fixture would not have loaded at all. The real
defect is what happens WITH `--features cron` compiled: composition returns a
`NoOpScheduler`, the loader passes, the wizard passes, and nothing is scheduled.
Story 19.12 collapsed a `#[cfg]` split whose two arms were byte-identical
`NoOpScheduler` — a split that lied about having two behaviours — and that
change is honest about its reach: it changes NOTHING a user sees. Filed as
DF-19-12-PROFILE-CRON-UNWIRED. Ravi's "scheduled checks every 2 hours" is
therefore NOT PROVEN and NOT PROVABLE in this build.

`export` WRITES TOML ONLY. The PRD calls the shared artifact "a portable TOML +
persona markdown"; `export.rs` has one `fs::write` and it writes TOML. Zero
`.md` files are produced anywhere. It also FLATTENS `extends` away — so the
exported artifact and the inheritance-bearing artifact are two different files,
by design, per the export header's own fourth line — and it orders its sections
ALPHABETICALLY, not in port order, despite the comment in
`profile_serializer.rs` claiming "canonical order". The exported file is not
byte-stable either: header line 3 is `chrono::Utc::now()`.

`rustain profile create devops` — the PRD's own Opening Scene command — DOES NOT
PARSE. `error: unexpected argument 'devops' found`, exit 2: `Create` has no
positional. The shipped forms are `rustain profile create` and
`… --name devops`, and this capture uses the latter. Classified as a DOC defect
and annotated in the PRD rather than "fixed" by adding a positional, which would
have made `--name` and a positional two ways to say one thing.

`install gh:user/profile-name` IS NOT EXERCISED. This story added the local-path
arm and did not touch the git route; FR73's network half remains uncaptured. The
one thing asserted about it here is that it still prints its own gh-spec error on
stderr, which is what keeps the local-path branch predicate honest.

`install`'s local-path arm now refuses built-in profile names before delegating
to `import`; direct `import` still permits an explicit overwrite after its
interactive prompt. That asymmetry is deliberate: install means accepting
someone else's profile, while import means loading a file the operator owns.

FOR J8, A "PANE" IS A CAPTURED CLI OUTPUT BLOCK, not a TUI pane. The driver
emits the same labelled fence bytes around each child process so the shared
checker can use its one `pane_slice` extractor without a second implementation.

THE TURN'S REPLY IS STUB-SERVED AND IS ASSERTED NOWHERE. The scene decides what
the model says. What the gate asserts about beat 10 is that the turn HAPPENED
under the installed profile: a scene-turn row on the request log under the
declared persona, the ledger millisecond the product minted, and the FILTERED
wire tool count (12, against the default `coding`'s 15) that only
`[tools] adapter = "builtin-only"` can produce. Without that third conjunct the
marker could not go red, because `devops` extends `coding` and `coding` is the
product default — dropping `RUSTAIN_PROFILE` would compose an agent that is
byte-identical on the wire.

`profile show`'s `Source:` LINE CAN NEVER SAY `community`. `show.rs` returns
`User` for any non-embedded name, verified against a profile living only in
`community/`. Anyone reaching for `Source:` to prove where a file landed gets a
green that means nothing; the gate reads the PATH.
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
  capture_complete || EXIT_OR=1
else
  say "not captured — the walk or the stub reported a failure (exit $EXIT_OR)"
fi
exit "$EXIT_OR"
