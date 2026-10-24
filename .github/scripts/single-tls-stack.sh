#!/usr/bin/env bash
# Single TLS stack check (Story 18.7 AC1). Reads the output of
# `cargo tree --locked -e normal --prefix none --format '{p}'` on stdin and
# fails unless ring is present and no second TLS/crypto stack is linked.
# $1 names the feature set in diagnostics.
set -euo pipefail

FEATURES=${1:-default}
TREE=$(cat)

if ! grep -Eq '^ring v' <<< "$TREE"; then
    echo "ERROR: ring missing from TLS stack (features: ${FEATURES})" >&2
    exit 1
fi
if grep -E '^(aws-lc-rs|aws-lc-sys|openssl-sys|native-tls|hyper-tls) v' <<< "$TREE" >&2; then
    echo "ERROR: second TLS stack present (features: ${FEATURES})" >&2
    exit 1
fi
