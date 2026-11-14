#!/usr/bin/env python3
"""Fail the Python TUI lane on a skip that tests_tui/skip-allowlist.json does not name.

Usage: check-tui-skips.py <junit.xml> <allowlist.json>

The allowlist is a JSON object `"<classname>::<name>": "<reason naming the owner row>"`,
keys exactly as pytest's JUnit file records them.
Exit 0: every skip is listed and every entry was skipped.
Exit 1: an unlisted skip, or a stale entry (listed but not skipped in this run).
Exit 2: unreadable or empty JUnit, or a malformed allowlist.
"""

import json
import sys
import xml.etree.ElementTree as ET


def escape(text: str) -> str:
    return text.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def _no_duplicates(pairs):
    keys = [key for key, _ in pairs]
    duplicates = sorted({key for key in keys if keys.count(key) > 1})
    if duplicates:
        raise ValueError(f"duplicate allowlist keys: {duplicates}")
    return dict(pairs)


def load_allowlist(path) -> dict:
    with open(path, encoding="utf-8") as handle:
        data = json.load(handle, object_pairs_hook=_no_duplicates)
    if not isinstance(data, dict):
        raise ValueError("allowlist must be a JSON object")
    for key, reason in data.items():
        if not isinstance(reason, str) or not reason.strip():
            raise ValueError(f"allowlist entry without a reason: {key}")
    return data


def skipped_keys(path) -> set:
    cases = ET.parse(path).getroot().iter("testcase")
    total = 0
    skipped = set()
    for case in cases:
        total += 1
        if case.find("skipped") is not None:
            skipped.add(f"{case.get('classname', '')}::{case.get('name', '')}")
    if total == 0:
        raise ValueError("JUnit file holds no testcases")
    return skipped


def skip_violations(junit_path, allowlist_path) -> dict:
    skipped = skipped_keys(junit_path)
    allowed = set(load_allowlist(allowlist_path))
    return {"unlisted": skipped - allowed, "stale": allowed - skipped}


def main(argv) -> int:
    if len(argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    try:
        violations = skip_violations(argv[0], argv[1])
    except (OSError, ET.ParseError, ValueError) as error:
        print(f"::error title=Python TUI skip check::{escape(str(error))}")
        return 2
    for key in sorted(violations["unlisted"]):
        print(f"::error title=Unlisted Python TUI skip::{escape(key)} is skipped but not in {argv[1]}")
    for key in sorted(violations["stale"]):
        print(f"::error title=Stale Python TUI skip entry::{escape(key)} is listed in {argv[1]} but was not skipped")
    unlisted, stale = len(violations["unlisted"]), len(violations["stale"])
    print(f"python TUI skip check: {unlisted} unlisted skip(s), {stale} stale entr(y/ies)")
    return 1 if unlisted or stale else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
