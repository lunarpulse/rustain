#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../../.."
test "$(tr -d '\n' < tests/fixtures/a2a/TEST_ONLY_ed25519_seed.hex)" = "$(printf '07%.0s' {1..32})"
cargo test --features a2a --test a2a_jws
