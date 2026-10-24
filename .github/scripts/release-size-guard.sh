#!/usr/bin/env bash
# Linux release ratchet (Story 18.7). Lower the reserve only in a reviewed
# feature change accompanied by a measured size table; C-2 consumes it when
# adding telegram,cron. The unchanged NFR9 ceiling is checked first.
set -euo pipefail

NFR9_CEILING_BYTES=39845888
RESERVED_HEADROOM_BYTES=5373952

if [ "$#" -ne 1 ] || [ ! -f "$1" ]; then
    echo "Usage: $0 <release-binary>" >&2
    exit 1
fi

# -L measures the file a symlink names; GNU stat first, BSD (macOS) fallback.
SIZE=$(stat -L -c%s -- "$1" 2>/dev/null || stat -L -f%z -- "$1")
LIMIT=$((NFR9_CEILING_BYTES - RESERVED_HEADROOM_BYTES))
echo "Binary size: ${SIZE} bytes; ratchet limit: ${LIMIT} bytes"
if [ "$SIZE" -gt "$NFR9_CEILING_BYTES" ]; then
    echo "ERROR: NFR9 ceiling exceeded (${SIZE} > ${NFR9_CEILING_BYTES} bytes)" >&2
    exit 1
fi
if [ "$SIZE" -gt "$LIMIT" ]; then
    echo "ERROR: RESERVED_HEADROOM_BYTES=${RESERVED_HEADROOM_BYTES} violated (${SIZE} > ${LIMIT} bytes)" >&2
    exit 1
fi
