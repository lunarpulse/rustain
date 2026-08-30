#!/usr/bin/env bash
set -euo pipefail

# Story 19.5: this release-leg smoke proves `a2a` positively by serving and
# fetching a signed AgentCard through the production front door. `p2p`'s
# executing positive remains gate 18, pre-tag and manual: the real front door
# requires two hosts, QUIC, `peer ping`, and a daemon, all intentionally excluded
# from this small hermetic tag-path check.

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <release-binary>" >&2
  exit 2
fi

for required in curl python3 realpath; do
  if ! command -v "$required" >/dev/null 2>&1; then
    echo "release smoke requires $required" >&2
    exit 2
  fi
done

BIN=$(realpath "$1")
if [[ ! -x "$BIN" ]]; then
  echo "release smoke binary is missing or not executable: $1" >&2
  exit 2
fi

RUN_DIR=$(mktemp -d)
SERVER_PID=""
cleanup() {
  if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  rm -rf "$RUN_DIR"
}
trap cleanup EXIT
trap 'cleanup; exit 130' INT TERM HUP

mkdir -p "$RUN_DIR/home" "$RUN_DIR/config" "$RUN_DIR/workspace"
PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')
CARD="$RUN_DIR/agent-card.json"
LOG="$RUN_DIR/a2a-server.log"
URL="http://127.0.0.1:${PORT}/.well-known/agent-card.json"

(
  cd "$RUN_DIR/workspace"
  exec env HOME="$RUN_DIR/home" RUSTAIN_CONFIG_DIR="$RUN_DIR/config" \
    "$BIN" --serve-a2a="127.0.0.1:${PORT}"
) >"$LOG" 2>&1 &
SERVER_PID=$!

# A deadline, not an attempt count: while nothing is listening `curl` fails
# instantly (connection refused), so 50 attempts x `sleep 0.1` is a ~5 s window
# that a cold start on a loaded runner can lose — a false red on the tag path.
READY_TIMEOUT=${RELEASE_SMOKE_TIMEOUT:-60}
HTTP_STATUS=""
READY_DEADLINE=$((SECONDS + READY_TIMEOUT))
while ((SECONDS < READY_DEADLINE)); do
  if HTTP_STATUS=$(curl -sS --max-time 1 -o "$CARD" -w '%{http_code}' "$URL" 2>/dev/null); then
    if [[ "$HTTP_STATUS" == "200" ]]; then
      break
    fi
  fi
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "release smoke: A2A server exited before serving its AgentCard" >&2
    cat "$LOG" >&2
    exit 1
  fi
  sleep 0.1
done

if [[ "$HTTP_STATUS" != "200" ]]; then
  echo "release smoke: expected HTTP 200 from $URL within ${READY_TIMEOUT}s, got ${HTTP_STATUS:-no response}" >&2
  cat "$LOG" >&2
  exit 1
fi

python3 -c 'import json, sys; card=json.load(open(sys.argv[1], encoding="utf-8")); signatures=card.get("signatures"); assert isinstance(signatures, list) and signatures, "AgentCard signatures missing"; assert any(isinstance(entry, dict) and isinstance(entry.get("signature"), str) and entry["signature"] for entry in signatures), "AgentCard signature value missing"' "$CARD"

echo "release smoke: PASS (a2a signed AgentCard HTTP 200; p2p positive = manual gate 18)"
