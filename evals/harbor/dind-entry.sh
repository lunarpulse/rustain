#!/bin/sh
# Entry point inside the docker:dind container that run-isolated.sh starts:
# brings up the nested Docker daemon, installs what run-baseline.sh needs and
# runs it. Everything it installs or pulls stays in that container.
#
# usage: dind-entry.sh <static-rustain-binary> <jobs-dir> [harbor options]
set -u

binary="$1"
jobs_dir="$2"
shift 2
here="$(cd "$(dirname "$0")" && pwd)"
daemon_log="$jobs_dir/dind-dockerd.log"

# The container runs as root; hand the results back to the jobs dir's owner,
# also when the run is interrupted.
trap 'chown -R "$(stat -c %u:%g "$jobs_dir")" "$jobs_dir"' EXIT
trap 'exit 130' INT TERM

# The bare `dockerd` argument keeps the daemon on its Unix socket; the image's
# default command would also listen on TCP. The daemon start mounts a fresh
# /tmp, so nothing before this line may use it.
dockerd-entrypoint.sh dockerd >"$daemon_log" 2>&1 &
daemon=$!
# The daemon gives no readiness signal to wait on, so poll it: at most 60
# times, and stop at once when it has exited.
tries=0
until docker info >/dev/null 2>&1; do
    tries=$((tries + 1))
    if [ "$tries" -ge 60 ] || ! kill -0 "$daemon" 2>/dev/null; then
        echo "dind-entry.sh: nested Docker daemon did not start; last log lines:" >&2
        tail -n 20 "$daemon_log" >&2
        exit 1
    fi
    sleep 1
done
docker info --format 'nested Docker {{.ServerVersion}}, storage driver {{.Driver}}'

# coreutils: uv's entry scripts call `realpath --`, which BusyBox rejects.
apk add --no-cache --quiet bash coreutils python3 uv || exit 1
"$here/run-baseline.sh" "$binary" "$jobs_dir" "$@"
