#!/usr/bin/env python3
"""Require a separate constant-named RATCHET-SIGNOFF for every ratchet change.

Compare Rust declaration tokens, ignoring only whitespace and comments. Signoffs
are line trailers in base..head commit messages or PR_BODY / --pr-body-file:
    RATCHET-SIGNOFF: EVENT_LOOP_RUN_BASELINE_CCN — rationale and reviewer
"""
import argparse
import os
from pathlib import Path
import re
import subprocess
import sys

SOURCE = "tests/conformance.rs"
PROTECTED = frozenset({
    "MAX_KNOWN_BYPASSES", "MAX_KNOWN_STD_SYNC_LOCKS",
    "MAX_KNOWN_DURABLE_ITEM_CLAIM_IDENTIFIERS", "EVENT_LOOP_BASELINE_LINES",
    "EVENT_LOOP_BASELINE_SHA", "EVENT_LOOP_SOFT_BUDGET", "EVENT_LOOP_HARD_BUDGET",
    "EXPECTED_HANDLE_COUNT", "EVENT_LOOP_RUN_BASELINE_CCN", "COMPLEXITY_MULTIPLIER_PCT",
})
WORD = re.compile(r"(?:r#)?[A-Za-z_][A-Za-z_0-9]*|[0-9][A-Za-z_0-9]*")
RAW_STRING = re.compile(r'(?:br|cr|r)(#*)"')
CHARACTER = re.compile(r"(?:b)?'(?:\\(?:u\{[0-9A-Fa-f_]+\}|x[0-9A-Fa-f]{2}|[^\r\n])|[^'\\\r\n])'")
SIGNOFF = re.compile(r"^[ \t]*RATCHET-SIGNOFF:[ \t]*([A-Za-z_][A-Za-z_0-9]*)(?=$|[ \t\r])", re.MULTILINE)


class GuardError(Exception):
    """An unreadable comparison must not become an unsigned pass."""


def git(*arguments, allowed=(0,)):
    result = subprocess.run(["git", *arguments], capture_output=True, text=True,
                            encoding="utf-8", errors="strict", check=False)
    if result.returncode not in allowed:
        raise GuardError(f"git {' '.join(arguments)} failed ({result.returncode}): {result.stderr.strip()}")
    return result.stdout


def tokens(source):
    """Skip nested Rust comments without treating literal contents as Rust code."""
    position = 0
    while position < len(source):
        if source[position].isspace():
            position += 1
        elif source.startswith("//", position):
            end = source.find("\n", position)
            position = len(source) if end < 0 else end + 1
        elif source.startswith("/*", position):
            depth = 1
            position += 2
            while depth:
                if position >= len(source):
                    raise GuardError("unterminated Rust block comment")
                if source.startswith("/*", position):
                    depth += 1
                    position += 2
                elif source.startswith("*/", position):
                    depth -= 1
                    position += 2
                else:
                    position += 1
        else:
            start = position
            raw = RAW_STRING.match(source, position)
            character = CHARACTER.match(source, position)
            if raw:
                closing = '"' + raw.group(1)
                end = source.find(closing, raw.end())
                if end < 0:
                    raise GuardError("unterminated Rust raw string")
                position = end + len(closing)
            elif source.startswith(('"', 'b"', 'c"'), position):
                position = source.index('"', position) + 1
                while position < len(source):
                    if source[position] == "\\":
                        position += 2
                    elif source[position] == '"':
                        position += 1
                        break
                    else:
                        position += 1
                else:
                    raise GuardError("unterminated Rust string")
            elif character:
                position = character.end()
            else:
                word = WORD.match(source, position)
                position = word.end() if word else position + 1
            yield source[start:position]


def declarations(source):
    items = list(tokens(source))
    found = {name: [] for name in PROTECTED}
    for index, item in enumerate(items[:-1]):
        name = items[index + 1].removeprefix("r#")
        if item != "const" or name not in PROTECTED:
            continue
        declaration = []
        delimiters = []
        for token in items[index:]:
            declaration.append(token)
            if token in ("(", "[", "{"):
                delimiters.append(token)
            elif token in (")", "]", "}"):
                if not delimiters or delimiters.pop() != {")": "(", "]": "[", "}": "{"}[token]:
                    raise GuardError(f"malformed declaration: {name}")
            elif token == ";" and not delimiters:
                break
        else:
            raise GuardError(f"unterminated declaration: {name}")
        if len(declaration) < 6 or declaration[2] != ":" or "=" not in declaration:
            raise GuardError(f"malformed declaration: {name}")
        found[name].append(tuple(declaration))
    return {name: sorted(values) for name, values in found.items()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", required=True, help="base commit/ref")
    parser.add_argument("--head", required=True, help="head commit/ref")
    parser.add_argument("--pr-body-file", type=Path, help="UTF-8 PR body; otherwise use PR_BODY")
    args = parser.parse_args()
    try:
        base = git("rev-parse", "--verify", "--end-of-options", f"{args.base}^{{commit}}").strip()
        head = git("rev-parse", "--verify", "--end-of-options", f"{args.head}^{{commit}}").strip()
        # Exercise the comparison as well as both reads; no failed git command is ignored.
        git("diff", "--no-ext-diff", "--exit-code", base, head, "--", SOURCE, allowed=(0, 1))
        before = declarations(git("show", f"{base}:{SOURCE}"))
        after = declarations(git("show", f"{head}:{SOURCE}"))
        messages = git("log", "--format=%B", f"{base}..{head}")
        body = args.pr_body_file.read_text(encoding="utf-8") if args.pr_body_file else os.environ.get("PR_BODY", "")
        signed = set(SIGNOFF.findall(messages + "\n" + body))
        changed = {name for name in PROTECTED if before[name] != after[name]}
        missing = changed - signed
    except (GuardError, OSError, UnicodeError) as error:
        print(f"Ratchet signoff guard failed closed: {error}", file=sys.stderr)
        return 2
    if missing:
        print("Missing a distinct RATCHET-SIGNOFF for each changed constant:", file=sys.stderr)
        for name in sorted(missing):
            print(f"  RATCHET-SIGNOFF: {name} — rationale and reviewer", file=sys.stderr)
        return 1
    print("Ratchet signoffs satisfied: " + (", ".join(sorted(changed)) if changed else "no protected declaration changed"))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
