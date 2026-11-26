#!/usr/bin/env bash
# Builds the static x86_64 musl rustain binary that the eval-4 run uploads into
# task containers. The release asset for x86_64 Linux is a glibc build that
# needs GLIBC_2.39, newer than the Debian 12 task images provide, so the run
# uses a musl build of the same commit with the release's own feature set.
#
# usage: build-musl.sh <source-checkout> <expected-commit> <output-binary>
#
# <source-checkout> must be a clean checkout of <expected-commit>. The feature
# list is read from that checkout's .github/workflows/release.yml, so the build
# has exactly the features the release ships. Set CARGO_TARGET_DIR to keep the
# build out of the checkout. Needs cargo-zigbuild; CARGO_ZIGBUILD_ZIG_PATH may
# name the zig to use.
#
# Writes <output-binary> and <output-binary>.sha256, and refuses a result that
# is not statically linked.
set -euo pipefail

usage="usage: build-musl.sh <source-checkout> <expected-commit> <output-binary>"
[ "$#" -eq 3 ] || { echo "$usage" >&2; exit 2; }
source_dir="$(cd "$1" && pwd)"
expected="$2"
output="$3"
target=x86_64-unknown-linux-musl

head="$(git -C "$source_dir" rev-parse HEAD)"
if [ "$head" != "$(git -C "$source_dir" rev-parse "$expected^{commit}")" ]; then
    echo "build-musl.sh: $source_dir is at $head, not $expected" >&2
    exit 1
fi
if [ -n "$(git -C "$source_dir" status --porcelain --untracked-files=no)" ]; then
    echo "build-musl.sh: $source_dir has local changes" >&2
    exit 1
fi

# The release builds with one `cargo build ... --features <list>` line.
features="$(sed -n 's/.*cargo build --release --target .* --features \([a-z0-9,-]*\).*/\1/p' \
    "$source_dir/.github/workflows/release.yml")"
if [ -z "$features" ] || [ "$(printf '%s\n' "$features" | wc -l)" -ne 1 ]; then
    echo "build-musl.sh: cannot read one release feature list from release.yml" >&2
    exit 1
fi
echo "build-musl.sh: $head, features $features"

(cd "$source_dir" && cargo zigbuild --locked --release --target "$target" \
    --features "$features" -j "${BUILD_JOBS:-4}")

built="${CARGO_TARGET_DIR:-$source_dir/target}/$target/release/rustain"
# A static binary has no program interpreter and no needed shared library.
if readelf -lW "$built" | grep -q 'Requesting program interpreter' ||
    readelf -dW "$built" | grep -q '(NEEDED)'; then
    echo "build-musl.sh: $built is not statically linked" >&2
    exit 1
fi

mkdir -p "$(dirname "$output")"
cp -- "$built" "$output"
chmod 755 "$output"
(cd "$(dirname "$output")" && sha256sum -- "$(basename "$output")" >"$(basename "$output").sha256")
cat "$output.sha256"
