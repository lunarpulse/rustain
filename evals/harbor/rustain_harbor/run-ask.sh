#!/usr/bin/env bash
# Runs one `rustain ask` turn in the current directory (the task workspace).
#
# usage: run-ask.sh <logs-dir> <instruction-file>
#
# The provider key arrives as a private file named by RUSTAIN_HARBOR_KEY_FILE;
# RUSTAIN_HARBOR_KEY_ENV names the variable rustain reads it from.
#
# stdout of rustain (stream-json) goes to <logs-dir>/rustain.stream.jsonl and
# stderr to <logs-dir>/rustain.stderr.txt. The sessions this run saves under
# ./.claude/sessions are moved to <logs-dir>/sessions, so the task verifier sees
# only what the agent did to the workspace. Exit status: rustain's, or 3 when
# rustain succeeded and a session could not be collected.
set -uo pipefail

logs="$1"
instruction_file="$2"

if [ -n "${RUSTAIN_HARBOR_KEY_FILE:-}" ]; then
    export "${RUSTAIN_HARBOR_KEY_ENV:?}=$(<"$RUSTAIN_HARBOR_KEY_FILE")"
    rm -f -- "$RUSTAIN_HARBOR_KEY_FILE"
fi

had_claude=0
[ -e .claude ] && had_claude=1
had_sessions=0
[ -e .claude/sessions ] && had_sessions=1
sessions_before="$(ls -A .claude/sessions 2>/dev/null)"

rustain ask --yolo --output-format stream-json "$(cat "$instruction_file")" \
    >"$logs/rustain.stream.jsonl" 2>"$logs/rustain.stderr.txt"
status=$?

# Collect the sessions this run added. One that cannot be copied stays where
# it is: losing it would lose the transcript.
uncollected=()
while IFS= read -r name; do
    [ -n "$name" ] || continue
    grep -Fxq -- "$name" <<<"$sessions_before" && continue
    if mkdir -p "$logs/sessions" && cp -r -- ".claude/sessions/$name" "$logs/sessions/"; then
        rm -rf -- ".claude/sessions/$name"
    else
        echo "run-ask.sh: could not collect session $name; it stays in the workspace" >&2
        uncollected+=(".claude/sessions/$name")
    fi
done < <(ls -A .claude/sessions 2>/dev/null)
if [ "$had_sessions" = 0 ]; then
    rmdir .claude/sessions 2>/dev/null
    [ "$had_claude" = 0 ] && rmdir .claude 2>/dev/null
fi

# rustain's shell tool inherits this environment, so an agent command such as
# `env` puts the provider key into the stream and the saved session. Keep it
# out of everything this run wrote.
while IFS='=' read -r name value; do
    case "$name" in *_API_KEY) ;; *) continue ;; esac
    [ "${#value}" -ge 8 ] || continue
    while IFS= read -r file; do
        content="$(<"$file")"
        printf '%s\n' "${content//"$value"/"[redacted:$name]"}" >"$file"
    done < <(grep -rlIF -- "$value" "$logs" ${uncollected[@]+"${uncollected[@]}"} 2>/dev/null)
done < <(env)

# Harbor classifies provider failures from the command output.
tail -c 2000 "$logs/rustain.stderr.txt" >&2
if [ "$status" = 0 ] && [ "${#uncollected[@]}" -gt 0 ]; then
    exit 3
fi
exit "$status"
