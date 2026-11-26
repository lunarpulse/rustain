#!/usr/bin/env bash
# Eval-4 launcher. Runs the frozen run plan eval4/run-plan.json: every task of
# the cmp-benign-v1 manifest x every treatment x attempts 1..3 is one slot, and
# every slot is its own Harbor job, run one at a time under the plan's spend cap.
#
# usage: run-baseline.sh check <static-rustain-binary> <jobs-dir> <run-id>
#        run-baseline.sh run <static-rustain-binary> <jobs-dir> <run-id>
#        run-baseline.sh aggregate <jobs-dir> <run-id>
#
# check:     offline; validates the plan, manifest, price table, lock and the
#            binary's hash and prints the run header and every slot's argv.
#            Writes nothing. Needs python3 >= 3.12 only.
# run:       installs Harbor from uv.lock into <jobs-dir>/.rustain-harbor/venv,
#            writes <jobs-dir>/<run-id>/header.json (refused if the run id was
#            used), checks the provider keys, runs the slots, then aggregates.
#            Needs Docker, uv and the treatments' key (DEEPSEEK_API_KEY). Run
#            it where Docker is available; task containers cannot be started
#            from inside the devcontainer.
# aggregate: offline; reconciles the run against its header and prints the
#            results, or refuses with every missing, duplicate, unexpected or
#            unidentifiable slot. Needs python3 >= 3.12 only.
#
# No other argument is accepted: every setting is in the run plan and the
# header, so the argv recorded per slot is the argv Harbor ran.
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
usage="usage: run-baseline.sh {check|run} <static-rustain-binary> <jobs-dir> <run-id>
       run-baseline.sh aggregate <jobs-dir> <run-id>"

case "${1:-} $#" in
    "check 4")
        PYTHONPATH="$here" exec python3 -P -m rustain_harbor.launch check "$2" "$3" "$4" ;;
    "aggregate 3")
        PYTHONPATH="$here" exec python3 -P -m rustain_harbor.baseline "$2" "$3" ;;
    "run 4") ;;
    *)
        echo "$usage" >&2
        exit 2 ;;
esac
binary="$2"
jobs_dir="$3"
run_id="$4"

# No fallback install: this script must not write a Harbor environment into
# the caller's home.
if ! command -v uv >/dev/null 2>&1; then
    echo "run-baseline.sh: uv not found" >&2
    exit 1
fi
mkdir -p "$jobs_dir/.rustain-harbor" || exit 1
jobs_dir="$(cd "$jobs_dir" && pwd)"
# Harbor and every dependency at the versions and hashes uv.lock records. The
# project itself is not installed; PYTHONPATH serves it from this directory.
venv="$jobs_dir/.rustain-harbor/venv"
UV_PROJECT_ENVIRONMENT="$venv" uv sync --locked --no-install-project --project "$here" ||
    exit 1

# -P: the caller's working directory never shadows this harness.
PYTHONPATH="$here" "$venv/bin/python" -P -m rustain_harbor.launch run \
    "$binary" "$jobs_dir" "$run_id"
status=$?

# Slots that ran keep their results; aggregation reports what is missing.
PYTHONPATH="$here" "$venv/bin/python" -P -m rustain_harbor.baseline "$jobs_dir" "$run_id"
summary_status=$?

[ "$status" -ne 0 ] && exit "$status"
exit "$summary_status"
