#!/usr/bin/env bash
# Linux release ratchet (Story 18.7). Locked four-feature release, rustc
# 1.98.1, mold, Linux: baseline f5333224 34,441,680 B; Lane B 64a718db
# 36,700,776 B; delta +2,259,096 B; NFR9 39,845,888 B; limit 37,289,984 B.
# Lane B (epic 20, auth) consumed 2,818,048 B of the C-2 reserve; the
# 2,555,904 B remainder is rounded down to 64 KiB with >=512 KiB growth.
# Epic 21 (TUI modals, 21-1..21-13) grew the binary to 37,339,664 B at 21-13
# (+638,888 B over Lane B; 21-3 measured 37,017,904 B), 49,680 B over the old
# limit. Owner decision 2026-11-07: lower the reserve by 1 MiB (2,555,904 B ->
# 1,507,328 B; limit 38,338,560 B, 998,896 B above the 21-13 binary) for the
# rest of Epic 21; re-measure at each later story. Lower the reserve only in a
# reviewed feature change with a measured table; C-2 also consumes it when
# adding telegram,cron. NFR9 is checked first.
set -euo pipefail

NFR9_CEILING_BYTES=39845888
RESERVED_HEADROOM_BYTES=1507328

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
