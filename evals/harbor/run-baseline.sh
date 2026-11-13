#!/usr/bin/env bash
# Terminal-Bench 2 cost baseline for rustain: five fixed tasks, one attempt each.
#
# usage: run-baseline.sh <static-rustain-binary> <jobs-dir> [extra `harbor run` options]
#
# Needs Docker, uv, and the provider key in the environment (DEEPSEEK_API_KEY
# for the default model). Run it where Docker is available; task containers
# cannot be started from inside the devcontainer.
set -uo pipefail

usage="usage: run-baseline.sh <static-rustain-binary> <jobs-dir> [harbor options]"
binary="${1:?$usage}"
jobs_dir="${2:?$usage}"
shift 2

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
model="${RUSTAIN_EVAL_MODEL:-deepseek/deepseek-v4-flash}"
job_name="${HARBOR_JOB_NAME:-rustain-baseline-$(date -u +%Y%m%dT%H%M%SZ)}"

# random.Random(20261113).sample(sorted(task names), 5) over the 89 tasks of
# terminal-bench/terminal-bench-2.
tasks=(
    break-filter-js-from-html
    crack-7z-hash
    extract-elf
    mcmc-sampling-stan
    torch-pipeline-parallelism
)
include=()
for task in "${tasks[@]}"; do
    include+=(--include-task-name "$task" --include-task-name "*/$task")
done

# No fallback install: this script must not write a Harbor environment into
# the caller's home.
if ! command -v uvx >/dev/null 2>&1; then
    echo "run-baseline.sh: uvx not found" >&2
    exit 1
fi
harbor=(uvx --from "harbor>=0.25,<0.26" --with-editable "$here")

# A rejected key would otherwise fail every trial, after the images are pulled.
"${harbor[@]}" python -m rustain_harbor.preflight "$model" || exit 1

# Record the job directory as created by this script; clean-host.sh removes
# only what is recorded here.
mkdir -p "$jobs_dir/.rustain-harbor" || exit 1
printf '%s\n' "$job_name" >>"$jobs_dir/.rustain-harbor/jobs"

"${harbor[@]}" harbor run \
    --dataset terminal-bench/terminal-bench-2 \
    --agent rustain_harbor:RustainAgent \
    --model "$model" \
    --ak "binary_path=$binary" \
    --n-attempts 1 \
    --n-concurrent 2 \
    --jobs-dir "$jobs_dir" \
    --job-name "$job_name" \
    "${include[@]}" \
    "$@"
status=$?

# Failed trials still carry the usage of the calls they completed.
"${harbor[@]}" python -m rustain_harbor.baseline "$jobs_dir/$job_name" \
    --json "$jobs_dir/$job_name/baseline.json"
summary_status=$?

# Harbor's failure first; otherwise a baseline that could not be produced.
[ "$status" -ne 0 ] && exit "$status"
exit "$summary_status"
