#!/usr/bin/env bash
# Runs `run-baseline.sh run` inside one disposable Docker-in-Docker container,
# so the machine keeps nothing but the jobs directory: Harbor, its caches, the
# task images and the task containers live in that container and go with it.
#
# usage: run-isolated.sh <static-rustain-binary> <jobs-dir> <run-id>
#            [--accept-unknown-spend <run-id>/<slot-key>,...]
#
# Needs Docker and the provider keys the run plan's treatments name
# (`key_env` in eval4/run-plan.json; DEEPSEEK_API_KEY for both treatments).
# Only those keys enter the container. The container is privileged: a nested
# Docker daemon cannot run otherwise.
set -uo pipefail

usage="usage: run-isolated.sh <static-rustain-binary> <jobs-dir> <run-id> [--accept-unknown-spend <run-id>/<slot-key>,...]"
case "$#" in
    3) ;;
    5) [ "$4" = --accept-unknown-spend ] || { echo "$usage" >&2; exit 2; } ;;
    *) echo "$usage" >&2; exit 2 ;;
esac
binary="$1"
jobs_dir="$2"
run_id="$3"
override=("${@:4}")

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# By digest: the tag's OCI index digest, read from Docker Hub's registry
# (HEAD /v2/library/docker/manifests/29.9.3-dind) and tag API on 2026-11-26.
image="docker:29.9.3-dind@sha256:7bf70dcb85c822e3fa6f2d332143e474b99147b9d7b39102b0cfe8b04a7dd61b"
name="rustain-harbor-eval-$$"
mapfile -t key_vars < <(
    sed -n 's/.*"key_env": *"\([A-Z0-9_]*\)".*/\1/p' "$here/eval4/run-plan.json" | sort -u
)
if [ "${#key_vars[@]}" -eq 0 ]; then
    echo "run-isolated.sh: eval4/run-plan.json names no key_env" >&2
    exit 1
fi
run_args=()
for key_var in "${key_vars[@]}"; do
    if [ -z "${!key_var:-}" ]; then
        echo "run-isolated.sh: $key_var is not set" >&2
        exit 1
    fi
    run_args+=(--env "$key_var")
done
binary="$(realpath -e "$binary")" || exit 1
mkdir -p "$jobs_dir" || exit 1
jobs_dir="$(cd "$jobs_dir" && pwd)"

# Leave the image store as it was found. The record lets clean-host.sh remove
# the image if this script is killed before it can.
pulled_record="$jobs_dir/.rustain-harbor/pulled-image"
image_was_present=0
docker image inspect "$image" >/dev/null 2>&1 && image_was_present=1
if [ "$image_was_present" = 0 ]; then
    mkdir -p "$(dirname "$pulled_record")" || exit 1
    printf '%s\n' "$image" >"$pulled_record"
fi
cleanup() {
    docker rm --force --volumes "$name" >/dev/null 2>&1
    if [ "$image_was_present" = 0 ] && docker rmi "$image" >/dev/null 2>&1; then
        rm -f -- "$pulled_record"
    fi
}
trap cleanup EXIT

# On a terminal, Ctrl-C reaches the run through the container's TTY.
[ -t 0 ] && [ -t 1 ] && run_args+=(--interactive --tty)

# --init keeps the entry script from being PID 1, which would ignore signals;
# TINI_KILL_PROCESS_GROUP makes a forwarded signal reach Harbor as well.
docker run --rm --privileged --init --name "$name" \
    --label rustain-harbor-eval=1 \
    --env TINI_KILL_PROCESS_GROUP=1 \
    "${run_args[@]}" \
    --volume "$here:$here:ro" \
    --volume "$binary:$binary:ro" \
    --volume "$jobs_dir:$jobs_dir" \
    --entrypoint sh \
    "$image" "$here/dind-entry.sh" "$binary" "$jobs_dir" "$run_id" \
    ${override[@]+"${override[@]}"}
