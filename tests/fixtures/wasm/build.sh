#!/usr/bin/env bash
# Story 17.3a — reproducible build recipe for the WASM sandbox fixtures.
#
# Party ruling F3: a committed `.wasm` blob is un-greppable supply-chain
# surface. So each fixture commits its SOURCE (`src/<name>.wat`, WebAssembly
# component text) + this recipe; the `.wasm` bytes are rebuilt from source and
# pinned by sha256 in `manifest.json`. No wasm toolchain runs in CI — the test
# only asserts the committed hashes and executes the committed bytes.
#
# Requires `wasm-tools` (https://github.com/bytecodealliance/wasm-tools):
#   cargo install wasm-tools
# Verified with wasm-tools 1.252.0.
#
# Usage:  ./build.sh          # rebuild every fixture + print sha256
set -euo pipefail
cd "$(dirname "$0")"

for f in src/*.wat; do
  name="$(basename "$f" .wat)"
  wasm-tools parse "$f" -o "$name.wasm"
  wasm-tools validate --features=component-model "$name.wasm"
done

echo "== sha256 (paste into manifest.json) =="
sha256sum ./*.wasm
