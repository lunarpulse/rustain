#!/usr/bin/env bash
# Linux locked release measurements at base 1943331f, rustc 1.98.1, mold:
# four-feature B4 38,012,720 B; +telegram 39,234,072 B (+1,221,352);
# +cron 38,165,896 B (+153,176); shipped six-feature B6 39,386,672 B
# (+1,373,952). B6 exceeds the 38 MiB fit threshold 39,321,600 B, so D-25
# applies the 40 MiB NFR9 ceiling. Reserve is calculated for B6 + 65,536 B:
# 1,966,080 B, leaving a 39,976,960 B limit and 590,288 B above B6.
# Recalibrate only with a reviewed measured table; check NFR9 first.
set -euo pipefail

NFR9_CEILING_BYTES=41943040
RESERVED_HEADROOM_BYTES=1966080

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
