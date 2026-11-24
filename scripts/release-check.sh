#!/usr/bin/env bash
# Release readiness check: RELEASING.md "Per-Release Checklist".
# Read-only: git, the anonymous GitHub API (curl + jq) and RELEASING.md. It reads
# no secret, needs no token and changes nothing.
#
#   scripts/release-check.sh vX.Y.Z            # from a clean checkout of the release commit
#   scripts/release-check.sh --sha SHA vX.Y.Z  # dry run: check SHA, skip the clean-tree check
#
# Prints one PASS, FAIL or UNKNOWN line per check, then the owner-only steps.
# Exit 0: every check passed. 1: a check failed. 2: nothing failed, but a hosted
# run is still in progress or the GitHub API could not be read (rate limit).
set -uo pipefail

usage() { echo "usage: $0 [--sha SHA] vX.Y.Z" >&2; exit 64; }
SHA_ARG=
if [ "${1:-}" = "--sha" ]; then
    [ "$#" -ge 2 ] || usage
    SHA_ARG=$2
    shift 2
fi
[ "$#" -eq 1 ] || usage
TAG=$1
[[ $TAG =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || usage
VERSION=${TAG#v}
cd "$(dirname "$0")/.." || exit 1

FAILS=0
UNKNOWNS=0
pass() { echo "PASS     $*"; }
fail() { echo "FAIL     $*"; FAILS=$((FAILS + 1)); }
unknown() { echo "UNKNOWN  $*"; UNKNOWNS=$((UNKNOWNS + 1)); }

SHA=$(git rev-parse --verify --quiet "${SHA_ARG:-HEAD}^{commit}") || { echo "not a commit: ${SHA_ARG:-HEAD}" >&2; exit 64; }
echo "release-check $TAG at $SHA"

# (a) The release commit is origin/main HEAD, and names the tag's version.
if [ -n "$SHA_ARG" ]; then
    echo "SKIP     clean checkout (dry run with --sha)"
elif [ -n "$(git status --porcelain --untracked-files=no)" ]; then
    fail "clean checkout: tracked files differ from HEAD"
else
    pass "clean checkout"
fi
MAIN=$(git ls-remote origin refs/heads/main | cut -f1)
if [ -z "$MAIN" ]; then
    unknown "origin/main HEAD: git ls-remote origin failed"
elif [ "$MAIN" = "$SHA" ]; then
    pass "commit is origin/main HEAD"
else
    fail "commit is origin/main HEAD (origin/main is $MAIN)"
fi
CRATE=$(git show "$SHA:Cargo.toml" | awk -F'"' '/^\[package\]/ { p = 1; next } /^\[/ { p = 0 } p && /^version *=/ { print $2; exit }')
if [ "$CRATE" = "$VERSION" ]; then pass "Cargo.toml version $CRATE"; else fail "Cargo.toml version is '$CRATE', tag needs $VERSION"; fi
DEFAULT=$(git show "$SHA:dist/install.sh" | sed -n 's/^DEFAULT_VERSION=//p')
if [ "$DEFAULT" = "$VERSION" ]; then
    pass "dist/install.sh DEFAULT_VERSION $DEFAULT"
else
    fail "dist/install.sh DEFAULT_VERSION is '$DEFAULT', release.yml publishes it only for $VERSION"
fi

# (e) The tag does not exist yet on origin.
if ! REMOTE_TAG=$(git ls-remote --tags origin "refs/tags/$TAG"); then
    unknown "tag $TAG absent on origin: git ls-remote origin failed"
elif [ -n "$REMOTE_TAG" ]; then
    fail "tag $TAG already exists on origin"
else
    pass "tag $TAG absent on origin"
fi

# (c) Quarantined tests: at most five, none past its date.
TODAY=$(date -u +%F)
QUARANTINE=$(git grep -h -E '^[[:space:]]*#\[ignore = "quarantine' "$SHA" -- '*.rs')
COUNT=$(printf '%s' "$QUARANTINE" | grep -c .)
STALE=0
while IFS= read -r line; do
    [ -n "$line" ] || continue
    until=$(printf '%s\n' "$line" | sed -nE 's/.* until ([0-9]{4}-[0-9]{2}-[0-9]{2})".*/\1/p')
    if [ -z "$until" ] || [[ $until < $TODAY ]]; then
        echo "         stale: $(printf '%s' "$line" | sed 's/^[[:space:]]*//')"
        STALE=$((STALE + 1))
    fi
done <<<"$QUARANTINE"
if [ "$COUNT" -gt 5 ] || [ "$STALE" -gt 0 ]; then
    fail "quarantine: $COUNT entries (at most 5), $STALE past their date or undated"
else
    pass "quarantine: $COUNT entries, none past its date ($TODAY)"
fi

# (b, d) Hosted runs for this exact commit.
REPO=$(git remote get-url origin | sed -nE 's#^(git@github\.com:|https://github\.com/)([^/]+/[^/]+)$#\2#p')
REPO=${REPO%.git}
# GET from the GitHub REST API; returns 1 (and prints why) on any non-200.
api() {
    local out code
    out=$(curl -sS --max-time 30 -w '\n%{http_code}' -H 'Accept: application/vnd.github+json' \
        "https://api.github.com/repos/$REPO/$1") || { echo "         curl failed: $1" >&2; return 1; }
    code=${out##*$'\n'}
    [ "$code" = 200 ] || { echo "         GitHub API HTTP $code (rate limit or access): $1" >&2; return 1; }
    printf '%s' "${out%$'\n'*}"
}
# Verdict for "status conclusion url": completed+success PASS, completed FAIL, else UNKNOWN.
verdict() {
    local what=$1 status=$2 conclusion=$3 url=$4
    if [ "$status" = none ]; then fail "$what: no run for $SHA"
    elif [ "$status" != completed ]; then unknown "$what: $status $url"
    elif [ "$conclusion" = success ]; then pass "$what: success $url"
    else fail "$what: $conclusion $url"
    fi
}
if [ -z "$REPO" ] || ! RUNS=$(api "actions/runs?head_sha=$SHA&per_page=100"); then
    unknown "CI on main: GitHub API not readable (rate limit, access or non-GitHub origin)"
    unknown "Nightly proofs and its Shipped release set job: GitHub API not readable"
else
    pick='if . == null then "none - - -" else "\(.status) \(.conclusion // "-") \(.html_url) \(.id)" end'
    # Any push run of ci.yml for this exact commit counts (main or its gate branch); a successful run wins.
    read -r st co url _ < <(jq -r "[.workflow_runs[] | select(.path == \".github/workflows/ci.yml\" and .event == \"push\")] | sort_by(.created_at) | (map(select(.conclusion == \"success\")) | last) // last | $pick" <<<"$RUNS")
    verdict "CI on this commit" "$st" "$co" "$url"
    # Any trigger counts; a successful run wins over a later failed or running one.
    read -r st co url id < <(jq -r "[.workflow_runs[] | select(.path == \".github/workflows/nightly-proofs.yml\")] | sort_by(.created_at) | (map(select(.conclusion == \"success\")) | last) // last | $pick" <<<"$RUNS")
    verdict "Nightly proofs" "$st" "$co" "$url"
    # (d) NFR9 ceiling, size reserve, TLS stack and smoke live in this job; not re-run here.
    if [ "$st" = none ]; then
        fail "Shipped release set job: no Nightly proofs run for $SHA"
    elif JOBS=$(api "actions/runs/$id/jobs?per_page=100"); then
        read -r st co url < <(jq -r '[.jobs[] | select(.name | startswith("Shipped release set"))] | last | if . == null then "none - -" else "\(.status) \(.conclusion // "-") \(.html_url)" end' <<<"$JOBS")
        verdict "Shipped release set job" "$st" "$co" "$url"
    else
        unknown "Shipped release set job: GitHub API not readable"
    fi
fi

# (f) Owner-only steps no script can check (RELEASING.md "Per-Release Checklist").
echo
echo "Owner-only steps (RELEASING.md):"
OWNER=$(awk '/^## Per-Release Checklist/ { f = 1; next } /^## / { f = 0 } f && /^- \[ \] / { sub(/^- \[ \] /, ""); print "  [ ] " $0 }' RELEASING.md)
if [ -n "$OWNER" ]; then echo "$OWNER"; else fail "RELEASING.md has no Per-Release Checklist items"; fi

echo
if [ "$FAILS" -gt 0 ]; then
    echo "NOT READY: $FAILS FAIL, $UNKNOWNS UNKNOWN"
    exit 1
elif [ "$UNKNOWNS" -gt 0 ]; then
    echo "NOT YET KNOWN: $UNKNOWNS UNKNOWN; re-run when the hosted runs finish"
    exit 2
fi
echo "READY: every check passed; the owner steps above remain"
