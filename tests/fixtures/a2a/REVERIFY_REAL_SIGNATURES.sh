#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../../.."
cargo test --features a2a --test a2a_fixtures both_captured_real_card_shapes_verify_offline -- --exact
