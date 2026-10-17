#!/usr/bin/env bash
# Canonical receipt consistency checker. This does not authenticate journal rows;
# executing capture gates, not committed text, establish runtime behavior.
# Usage: check-receipt.sh [--product-root ROOT] [--legacy-demo-root ROOT] RECEIPT
# Legacy demos belong to the actual outer consumer and are never guessed here.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
RUSTAIN_ROOT="$(cd "$HERE/../.." && pwd)"
LEGACY_DEMO_ROOT=""
usage() {
  printf 'usage: check-receipt.sh [--product-root ROOT] [--legacy-demo-root ROOT] RECEIPT\n' >&2
  exit 2
}
while [ "$#" -gt 0 ]; do
  case "$1" in
    --product-root|--legacy-demo-root)
      [ "$#" -ge 2 ] || usage
      [ -d "$2" ] || { printf 'FAIL: root is not a directory: %s\n' "$2" >&2; exit 2; }
      root="$(cd "$2" && pwd)" || exit 2
      if [ "$1" = --product-root ]; then RUSTAIN_ROOT="$root"; else LEGACY_DEMO_ROOT="$root"; fi
      shift 2 ;;
    --*) usage ;;
    *) break ;;
  esac
done
[ "$#" -eq 1 ] || usage
[ -f "$1" ] && [ -r "$1" ] || { printf 'FAIL: receipt is not a readable file: %s\n' "$1" >&2; exit 1; }
RECEIPT="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
FAILED=0
fail() { printf '  FAIL  %s\n' "$*"; FAILED=1; }
pass() { printf '  PASS  %s\n' "$*"; }

verify_receipt() { # file label
  local file="$1" label="$2"
  local recorded window_start window_end bad=0 total=0 unit shape
  case "$(basename "$file")" in
    federation-two-host-*.transcript.txt | j10-spine-same-host-*.transcript.txt)
      unit="frame correlation"; shape='peer-ping-[0-9]+-[0-9]{13}' ;;
    story-19-19-act2-*.transcript.txt | story-19-21-act1-*.transcript.txt | story-19-21b-conflict-flag-*.transcript.txt)
      unit="journalled millisecond"; shape='"recorded_at_ms": [0-9]{13}' ;;
    two-host-a2a-*.transcript.txt)
      # Story 18.8: gate 20's receipt is an A2A transcript like gate 19's, so it
      # is checked on the same product-minted journal millisecond — minted by
      # `clamp_recorded_at_ms` (`infrastructure/subagent/node_journal.rs`) and
      # surfaced by `cli/team/log.rs`.
      # ⚠ The space after the colon is CORRECT here and its absence is correct
      # in the journey arm below: `team log --json` pretty-prints, the usage
      # ledger is compact JSONL. A reviewer will read the difference as drift.
      # ⚠ This stem has no covering glob, so it needs its own arm — unlike a new
      # journey, which the `journey-J[0-9]*` arm already covers. The receipt, the
      # driver and the gate section therefore land in ONE commit: 19.8 ran that
      # mutant and a receipt committed without its arm failed the sweep three
      # times over.
      unit="journalled millisecond"; shape='"recorded_at_ms": [0-9]{13}' ;;
    team-send-loopback-*.transcript.txt)
      # Story 18.9 gate 21 uses the same product-minted room-journal timestamp
      # as gate 20. The caller's message id is not an independent witness.
      unit="journalled millisecond"; shape='"recorded_at_ms": [0-9]{13}' ;;
    journey-J[0-9]*-*.transcript.txt)
      # Story 19.8: the journey-receipt shape is the usage ledger's
      # product-minted millisecond. Compact JSON — no space after the colon
      # (19.7 A10 pins this shape; the story's AC3 wrote it with a space,
      # which matches no ledger byte and is corrected here).
      # Story 19.9 AC3: ONE arm for every journey stem (J0, J1, J2, …) — the
      # ledger shape and the `scene:` header check below are identical for all
      # of them, and a per-journey arm is a copy waiting to drift.
      unit="ledger millisecond"; shape='"timestampMs":[0-9]{13}' ;;
    *)
      fail "$label: no receipt shape is defined for this filename stem, so nothing in it is checkable"
      return ;;
  esac
  recorded="$(sed -n 's/^recorded-at: //p' "$file" | head -1)"
  if [ -z "$recorded" ]; then
    fail "$label: no recorded-at header, so it cannot be checked at all"
    return
  fi
  window_start="$(date -u -d "$recorded - 1 hour" +%s 2>/dev/null)" || {
    fail "$label: recorded-at is not a timestamp"; return; }
  window_end="$(date -u -d "$recorded + 1 hour" +%s 2>/dev/null)"
  while read -r millis; do
    total=$((total + 1))
    local seconds=$((millis / 1000))
    if [ "$seconds" -lt "$window_start" ] || [ "$seconds" -gt "$window_end" ]; then
      bad=$((bad + 1))
    fi
  done < <(grep -oE "$shape" "$file" | grep -oE '[0-9]{13}$' | sort -u)
  if [ "$total" -eq 0 ]; then
    fail "$label: carries no ${unit} at all, so it records no run"
  elif [ "$bad" -gt 0 ]; then
    fail "$label: $bad of $total ${unit}s were minted outside the run it claims"
  else
    pass "$label: all $total ${unit}s were minted inside the run it claims"
  fi

  # A journey receipt additionally names its scene and the scene's sha256 in
  # the header (19.7 A12). A stubbed capture is replayable, not true; the
  # hash line is what lets a reader see exactly what the binary was fed.
  # Scenes evolve by VERSIONING (j3-marco.json, j3-marco-v2.json, …): a
  # prior version is never edited, so an honestly-pinned receipt verifies
  # against the file it names forever. A superseded receipt whose named file
  # has since been versioned on must still show its pinned BYTES present in
  # a sibling scene file — evidence, grepped, never assumed. Anything else
  # is a drift this sweep exists to catch. Product paths resolve at the explicit
  # product root. Historical demos resolve only at the genuine outer consumer's
  # --legacy-demo-root, tolerating its historical leading "demos/".
  case "$(basename "$file")" in
    story-19-19-act2-*.transcript.txt)
      local scene_line scene_path scene_sha resolve
      scene_line="$(sed -n 's/^scene: *//p' "$file" | head -1)"
      scene_path="${scene_line%% sha256:*}"
      scene_sha="${scene_line##* sha256:}"
      scene_sha="${scene_sha# }"
      resolve="$RUSTAIN_ROOT/$scene_path"
      if [ "$scene_path" != "tests_tui/journeys/j10/scenes/19-19-act2-v1.json" ] || [ ! -f "$resolve" ]; then
        fail "$label: the scene: header must name tests_tui/journeys/j10/scenes/19-19-act2-v1.json"
      elif [ "$(sha256sum "$resolve" | awk '{print $1}')" != "$scene_sha" ]; then
        fail "$label: scene hash mismatch"
      fi
      local source_line source_path source_sha source_resolve
      source_line="$(sed -n 's/^source: *//p' "$file" | head -1)"
      source_path="${source_line%% sha256:*}"
      source_sha="${source_line##* sha256:}"
      source_sha="${source_sha# }"
      source_resolve="$RUSTAIN_ROOT/$source_path"
      if [ "$source_path" != "tests_tui/test_story_19_19_act2_consultation.py" ] || [ ! -f "$source_resolve" ]; then
        fail "$label: the source: header must name tests_tui/test_story_19_19_act2_consultation.py"
      elif [ "$(sha256sum "$source_resolve" | awk '{print $1}')" != "$source_sha" ]; then
        fail "$label: source hash mismatch"
      fi
      ;;
    story-19-21-act1-*.transcript.txt)
      local scene_line scene_path scene_sha resolve
      scene_line="$(sed -n 's/^scene: *//p' "$file" | head -1)"
      scene_path="${scene_line%% sha256:*}"
      scene_sha="${scene_line##* sha256:}"
      scene_sha="${scene_sha# }"
      resolve="$RUSTAIN_ROOT/$scene_path"
      if [ "$scene_path" != "tests_tui/journeys/j10/scenes/19-21-act1-v1.json" ] || [ ! -f "$resolve" ]; then
        fail "$label: the scene: header must name tests_tui/journeys/j10/scenes/19-21-act1-v1.json"
      elif [ "$(sha256sum "$resolve" | awk '{print $1}')" != "$scene_sha" ]; then
        fail "$label: scene hash mismatch"
      fi
      local source_line source_path source_sha source_resolve
      source_line="$(sed -n 's/^source: *//p' "$file" | head -1)"
      source_path="${source_line%% sha256:*}"
      source_sha="${source_line##* sha256:}"
      source_sha="${source_sha# }"
      source_resolve="$RUSTAIN_ROOT/$source_path"
      if [ "$source_path" != "tests_tui/test_story_19_21_act1_distribution.py" ] || [ ! -f "$source_resolve" ]; then
        fail "$label: the source: header must name tests_tui/test_story_19_21_act1_distribution.py"
      elif [ "$(sha256sum "$source_resolve" | awk '{print $1}')" != "$source_sha" ]; then
        fail "$label: source hash mismatch"
      fi
      ;;
    story-19-21b-conflict-flag-*.transcript.txt)
      local scene_line scene_path scene_sha resolve
      scene_line="$(sed -n 's/^scene: *//p' "$file" | head -1)"
      scene_path="${scene_line%% sha256:*}"
      scene_sha="${scene_line##* sha256:}"
      scene_sha="${scene_sha# }"
      resolve="$RUSTAIN_ROOT/$scene_path"
      if [ "$scene_path" != "tests_tui/journeys/j10/scenes/19-21b-conflict-flag-v1.json" ] || [ ! -f "$resolve" ]; then
        fail "$label: the scene: header must name tests_tui/journeys/j10/scenes/19-21b-conflict-flag-v1.json"
      elif [ "$(sha256sum "$resolve" | awk '{print $1}')" != "$scene_sha" ]; then
        fail "$label: scene hash mismatch"
      fi
      local source_line source_path source_sha source_resolve
      source_line="$(sed -n 's/^source: *//p' "$file" | head -1)"
      source_path="${source_line%% sha256:*}"
      source_sha="${source_line##* sha256:}"
      source_sha="${source_sha# }"
      source_resolve="$RUSTAIN_ROOT/$source_path"
      if [ "$source_path" != "tests_tui/test_story_19_21b_conflict_flag.py" ] || [ ! -f "$source_resolve" ]; then
        fail "$label: the source: header must name tests_tui/test_story_19_21b_conflict_flag.py"
      elif [ "$(sha256sum "$source_resolve" | awk '{print $1}')" != "$source_sha" ]; then
        fail "$label: source hash mismatch"
      fi
      ;;
    journey-J[0-9]*-*.transcript.txt)
      local scene_line scene_path scene_sha resolve stem latest SCENE_RETAINED
      scene_line="$(sed -n 's/^scene: *//p' "$file" | head -1)"
      scene_path="${scene_line%% sha256:*}"
      scene_sha="${scene_line##* sha256:}"
      resolve="$RUSTAIN_ROOT/$scene_path"
      if [ -n "$LEGACY_DEMO_ROOT" ]; then
        resolve="$LEGACY_DEMO_ROOT/$scene_path"
        [ -f "$resolve" ] || resolve="$LEGACY_DEMO_ROOT/${scene_path#demos/}"
        [ -f "$resolve" ] || resolve="$RUSTAIN_ROOT/$scene_path"
      fi
      stem="$(basename "$file" | sed -E 's/^(journey-J[0-9]+)-.*/\1/')"
      latest="$(printf '%s\n' "$(dirname "$file")"/"$stem"-*.transcript.txt | sort | tail -1)"
      if [ -z "$scene_path" ] || [ ! -f "$resolve" ]; then
        fail "$label: the scene: header names no scene file that exists here"
      elif [ "$(sha256sum "$resolve" | awk '{print $1}')" = "$scene_sha" ]; then
        :
      elif [ "$file" != "$latest" ] && SCENE_RETAINED="$(cd "$(dirname "$resolve")" && sha256sum -- *.json 2>/dev/null | awk -v want="$scene_sha" '$1 == want { print $2; exit }')" && [ -n "$SCENE_RETAINED" ]; then
        pass "$label: its pinned scene bytes are retained in $(dirname "$scene_path")/$(basename "$SCENE_RETAINED"); $(basename "$latest") supersedes it"
      else
        fail "$label: scene hash mismatch"
      fi

      case "$(basename "$file")" in
        journey-J8-*.transcript.txt)
          local profile_line profile_path profile_sha profile_resolve
          profile_line="$(sed -n 's/^profile: *//p' "$file" | head -1)"
          profile_path="${profile_line%% sha256:*}"
          profile_sha="${profile_line##* sha256:}"
          profile_resolve="$RUSTAIN_ROOT/$profile_path"
          if [ -z "$profile_path" ] || [ ! -f "$profile_resolve" ]; then
            fail "$label: the profile: header names no profile file that exists here"
          elif [ "$(sha256sum "$profile_resolve" | awk '{print $1}')" != "$profile_sha" ]; then
            fail "$label: profile hash mismatch"
          fi
          ;;
      esac
      ;;
  esac
}

verify_receipt_stamp() { # file
  local receipt="$1" receipt_base receipt_stamp receipt_normalised receipt_header receipt_header_count
  receipt_base="$(basename "$receipt")"
  case "$receipt_base" in
    two-host-a2a-*) receipt_stamp="${receipt_base#two-host-a2a-}" ;;
    journey-J[0-9]*-*)
      case "$receipt_base" in
        journey-J0-2026-08-28T10-47-33Z.transcript.txt|journey-J3-2026-09-01T04-31-07Z.transcript.txt) return 0 ;;
      esac
      receipt_stamp="${receipt_base#journey-}"
      receipt_stamp="${receipt_stamp#*-}"
      ;;
    story-19-19-act2-*) receipt_stamp="${receipt_base#story-19-19-act2-}" ;;
    story-19-21-act1-*) receipt_stamp="${receipt_base#story-19-21-act1-}" ;;
    story-19-21b-conflict-flag-*) receipt_stamp="${receipt_base#story-19-21b-conflict-flag-}" ;;
    *) return 0 ;;
  esac
  receipt_stamp="${receipt_stamp%.transcript.txt}"
  receipt_normalised="$(printf '%s' "$receipt_stamp" | sed 's/T\(..\)-\(..\)-\(..\)Z/T\1:\2:\3Z/')"
  receipt_header="$(sed -n 's/^recorded-at: //p' "$receipt" | head -1)"
  receipt_header_count="$(grep -c '^recorded-at: ' "$receipt")"
  if [ "$receipt_header_count" -ne 1 ]; then
    fail "$receipt_base: carries $receipt_header_count recorded-at headers; exactly one is a record"
  elif [ "$receipt_normalised" = "$receipt_header" ]; then
    pass "$receipt_base: its filename stamp and its recorded-at header are the same instant"
  else
    fail "$receipt_base: filename stamp ($receipt_normalised) and recorded-at header ($receipt_header) disagree — one of them was edited by hand"
  fi
}
verify_receipt "$RECEIPT" "$(basename "$RECEIPT")"
verify_receipt_stamp "$RECEIPT"
[ "$FAILED" -eq 0 ]
