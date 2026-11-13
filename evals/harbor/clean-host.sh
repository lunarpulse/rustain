#!/usr/bin/env bash
# Removes what an interrupted run of these scripts left on a machine. Lists by
# default and removes only with --apply. It removes only what the scripts
# recorded as theirs:
#
#   - stopped containers that run-isolated.sh labelled
#   - with --jobs-dir <dir>: the docker:dind image, when a run recorded pulling
#     it there and was killed before it could remove it
#   - with --results as well: the job directories that run-baseline.sh recorded
#     in <dir>, the nested daemon's log and the record itself
#
# Harbor's own task images and cache, which run-baseline.sh leaves when it runs
# outside the container, belong to Harbor and may be shared with other Harbor
# work. They are reported, with the Harbor command that removes them.
#
# usage: clean-host.sh [--apply] [--jobs-dir <dir> [--results]]
set -uo pipefail

usage="usage: clean-host.sh [--apply] [--jobs-dir <dir> [--results]]"
apply=0
results=0
jobs_dir=""
while [ $# -gt 0 ]; do
    case "$1" in
        --apply) apply=1 ;;
        --results) results=1 ;;
        --jobs-dir) jobs_dir="${2:?$usage}"; shift ;;
        *) echo "$usage" >&2; exit 2 ;;
    esac
    shift
done
if [ "$results" = 1 ] && [ -z "$jobs_dir" ]; then
    echo "$usage" >&2
    exit 2
fi

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
record="${jobs_dir:+$jobs_dir/.rustain-harbor}"
found=0
failed=0

# remove <description> <command...>: lists the item, or runs the command with --apply.
remove() {
    local what="$1"
    shift
    found=1
    if [ "$apply" = 0 ]; then
        echo "would remove: $what"
    elif "$@" >/dev/null 2>&1; then
        echo "removed: $what"
    else
        echo "FAILED to remove: $what" >&2
        failed=1
    fi
}

remove_pulled_image() {
    docker rmi "$1" && rm -f -- "$record/pulled-image"
}

if command -v docker >/dev/null 2>&1; then
    # A running container is a run in progress, not a leftover.
    while IFS=$'\t' read -r name state; do
        [ -n "$name" ] || continue
        case "$state" in
            running | restarting | paused) echo "left as is (run in progress): container $name" ;;
            *) remove "container $name" docker rm --volumes "$name" ;;
        esac
    done < <(docker ps --all --filter label=rustain-harbor-eval --format '{{.Names}}\t{{.State}}')

    pulled=""
    [ -n "$record" ] && [ -f "$record/pulled-image" ] && pulled="$(head -n 1 "$record/pulled-image")"
    if [[ "$pulled" =~ ^docker:[0-9][0-9.]*-dind$ ]]; then
        if docker image inspect "$pulled" >/dev/null 2>&1; then
            # Without --force an image that a container still uses stays.
            remove "image $pulled (pulled by an interrupted run)" remove_pulled_image "$pulled"
        else
            remove "stale record of image $pulled" rm -f -- "$record/pulled-image"
        fi
    else
        dind_image="$(sed -nE 's/^image="(.+)"$/\1/p' "$here/run-isolated.sh")"
        if [ -n "$dind_image" ] && docker image inspect "$dind_image" >/dev/null 2>&1; then
            echo "left as is (no run recorded pulling it): image $dind_image"
        fi
    fi

    harbor_images="$(docker images --format '{{.Repository}}:{{.Tag}}' | grep -cE '^(alexgshaw/|hb__|sb__)')"
    if [ "$harbor_images" -gt 0 ]; then
        echo "left as is (Harbor's; \`harbor cache clean\` removes them): $harbor_images task image(s)"
    fi
else
    echo "docker not found; containers and images were not checked" >&2
fi
if [ -d "$HOME/.cache/harbor" ]; then
    echo "left as is (Harbor's; \`harbor cache clean\` removes it): $HOME/.cache/harbor"
fi

if [ "$results" = 1 ]; then
    if [ ! -f "$record/jobs" ]; then
        echo "clean-host.sh: $jobs_dir has no record of jobs created by these scripts; nothing removed" >&2
        failed=1
    else
        while IFS= read -r job; do
            # A job name is one path component; anything else is not ours to remove.
            case "$job" in '' | . | .. | */*) continue ;; esac
            [ -d "$jobs_dir/$job" ] && remove "directory $jobs_dir/$job (results)" rm -rf -- "$jobs_dir/$job"
        done < <(sort -u "$record/jobs")
        [ -f "$jobs_dir/dind-dockerd.log" ] && remove "file $jobs_dir/dind-dockerd.log" rm -f -- "$jobs_dir/dind-dockerd.log"
        # A record that still names an image this run could not remove stays,
        # so that a later run finds it.
        if [ "$apply" = 0 ] || [ ! -f "$record/pulled-image" ]; then
            remove "record $record" rm -rf -- "$record"
        fi
    fi
fi

[ "$found" = 1 ] || echo "nothing to clean"
[ "$found" = 1 ] && [ "$apply" = 0 ] && echo "run again with --apply to remove these"
exit "$failed"
