#!/usr/bin/env bash
# Regenerate FIXTURE_room_journal_pre_18_2.jsonl and re-pin its manifest hash.
#
# The generator is standalone on purpose: the current build cannot emit the
# pre-18.2 shape any more (it always stamps `recorded_at_ms` and `direction`),
# which is precisely why the shape has to be pinned rather than produced.
#
# Usage: ./REPRODUCE_transparency_fixture.sh
set -euo pipefail
cd "$(dirname "$0")"

PEER_A="12204bb06f8e4e3a7715d201d573d0aa423762e55dabd61a2c02278fa56cc6d294e0"
PEER_B="1220a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90"
ZERO_HASH="$(printf '0%.0s' {1..64})"
ONE_HASH="$(printf '1%.0s' {1..64})"
OUT="FIXTURE_room_journal_pre_18_2.jsonl"

line() { printf '{"schema_version":1,"seq":%s,"record":{"kind":"room","payload":%s}}\n' "$1" "$2"; }

{
  line 1 "{\"event\":\"node_registered\",\"node\":\"a2a-in/sub-a/t-1\",\"origin\":\"Remote\",\"host\":{\"host_id\":\"host-1\",\"workspace_id\":\"/ws\"}}"
  line 2 "{\"event\":\"remote_envelope_accepted\",\"peer\":\"${PEER_A}\",\"node\":\"a2a-in/sub-a/t-1\",\"content_hash\":\"${ZERO_HASH}\"}"
  line 3 "{\"event\":\"remote_envelope_rejected\",\"peer\":\"${PEER_A}\",\"reason\":{\"reason\":\"policy\",\"detail\":\"refused by server.admission policy\"}}"
  line 4 "{\"event\":\"remote_envelope_accepted\",\"peer\":\"${PEER_B}\",\"node\":\"a2a/peer-b/t-9\",\"content_hash\":\"${ONE_HASH}\"}"
  line 5 "{\"event\":\"remote_envelope_rejected\",\"peer\":\"${PEER_B}\",\"reason\":{\"reason\":\"policy\",\"detail\":\"peer reported terminal state failed\"}}"
  line 6 "{\"event\":\"admission_deferred\",\"coordinator\":\"root\",\"spoke\":\"${PEER_A}\",\"gate\":\"a2a-inbound-approval:t-2\"}"
  line 7 "{\"event\":\"admission_deferred\",\"coordinator\":\"root\",\"spoke\":\"${PEER_B}\",\"gate\":\"a2a-status-query:t-9\"}"
  line 8 "{\"event\":\"admission_deferred\",\"coordinator\":\"root\",\"spoke\":\"spoke-3\",\"gate\":\"fork-join-rate\"}"
  line 9 "{\"event\":\"peer_equivocated\",\"peer\":\"${PEER_B}\",\"topic\":\"t-topic\",\"heads\":[\"a\",\"b\"]}"
  line 10 "{\"event\":\"node_state_changed\",\"node\":\"a2a-in/sub-a/t-1\",\"from\":\"running\",\"to\":\"completed\"}"
} > "$OUT"

SHA="$(sha256sum "$OUT" | cut -d' ' -f1)"
cat > manifest.json <<JSON
{
  "${OUT}": {
    "sha256": "${SHA}",
    "provenance": {
      "shape_of": "rustain@6ca43eb .rustain/rooms/room-<hash>.jsonl",
      "authored": "2026-07-26",
      "story": "18.2"
    }
  }
}
JSON

echo "regenerated ${OUT} (sha256 ${SHA})"
