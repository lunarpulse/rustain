#!/usr/bin/env bash
# Runs run-baseline.sh inside one disposable Docker-in-Docker container, so the
# machine keeps nothing but the jobs directory: Harbor, its caches, the task
# images and the task containers live in that container and go with it.
#
# usage: run-isolated.sh <static-rustain-binary> <jobs-dir> [extra `harbor run` options]
#
# Needs Docker and the provider key in the environment (DEEPSEEK_API_KEY for
# the default model). The container is privileged: a nested Docker daemon
# cannot run otherwise.
set -uo pipefail

usage="usage: run-isolated.sh <static-rustain-binary> <jobs-dir> [harbor options]"
binary="${1:?$usage}"
jobs_dir="${2:?$usage}"
shift 2

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
image="docker:29.9.3-dind"
name="rustain-harbor-eval-$$"
# Same default as run-baseline.sh; only this provider's key enters the container.
model="${RUSTAIN_EVAL_MODEL:-deepseek/deepseek-v4-flash}"
provider="${model%%/*}"
key_var="${provider^^}_API_KEY"

if [ -z "${!key_var:-}" ]; then
    echo "run-isolated.sh: $key_var is not set" >&2
    exit 1
fi
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

run_args=(--env "$key_var" --env "RUSTAIN_EVAL_MODEL=$model")
[ -n "${HARBOR_JOB_NAME:-}" ] && run_args+=(--env HARBOR_JOB_NAME)
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
    "$image" "$here/dind-entry.sh" "$binary" "$jobs_dir" "$@"
